// Trained Uma export: your own veterans to <data dir>/trained_umas_<viewer id>.json, rewritten whenever the
// game loads or updates its veteran list. Read-only, local file only. Everything comes from the plain server
// models (Gallop.TrainedChara) the game hands to WorkTrainedCharaData; its work copies keep sparks, create
// time and chara id as Obscured*, which are never read here.
// The same hooks also keep each own veteran's two parents in memory for the veteran parents overlay.
use std::{collections::BTreeMap, ffi::CStr, ptr::null_mut, sync::{atomic::{AtomicBool, AtomicU32, Ordering}, Mutex}};

use serde_json::{json, Value};

use crate::{
    core::{utils, Hachimi},
    il2cpp::{
        api::{il2cpp_class_get_element_class, il2cpp_class_get_field_from_name, il2cpp_class_get_rank, il2cpp_class_get_type, il2cpp_field_get_type, il2cpp_type_get_type},
        ext::{Il2CppObjectExt, Il2CppStringExt},
        symbols::{get_field_object_value, get_field_value, get_method_overload_addr, invoke_object_method, Array},
        types::*
    }
};
use super::{DialogTrainedCharacterDetail::parent_slot, SuperSkip::lock, WorkDataManager};

// JSON key -> public Int32 field of Gallop.TrainedChara
const STATS: [(&str, &CStr); 5] = [("speed", c"speed"), ("stamina", c"stamina"), ("power", c"power"), ("guts", c"guts"), ("wit", c"wiz")];
// RaceDefine.ProperGrade: 1 = G .. 8 = S
const APTITUDES: [(&str, &CStr); 10] = [
    ("turf", c"proper_ground_turf"), ("dirt", c"proper_ground_dirt"),
    ("sprint", c"proper_distance_short"), ("mile", c"proper_distance_mile"), ("medium", c"proper_distance_middle"), ("long", c"proper_distance_long"),
    ("front", c"proper_running_style_nige"), ("pace", c"proper_running_style_senko"), ("late", c"proper_running_style_sashi"), ("end", c"proper_running_style_oikomi")
];

#[derive(Default)]
struct Store { viewer: Option<i64>, umas: BTreeMap<i32, Value> }
static STORE: Mutex<Store> = Mutex::new(Store { viewer: None, umas: BTreeMap::new() });
static BUSY: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicBool = AtomicBool::new(false); // a writer is spawned and hasn't taken its snapshot yet
static WRITE: Mutex<()> = Mutex::new(());

// An own veteran's parents: (card_id, owner_viewer_id) for parent 1 and 2. Memory only, never logged or written.
#[derive(Clone, Copy)]
pub(super) struct Vet { pub viewer: i64, pub card_id: i32, pub parents: [Option<(i32, i64)>; 2] }
static PARENTS: Mutex<BTreeMap<i32, Vet>> = Mutex::new(BTreeMap::new()); // by trained_chara_id
static GEN: AtomicU32 = AtomicU32::new(0); // bumped by every capture, so an open dialog rebuilds

pub(super) fn vet(id: i32) -> Option<Vet> {
    lock(&PARENTS).get(&id).copied()
}

pub(super) fn generation() -> u32 {
    GEN.load(Ordering::Acquire)
}

// UpdateAll replaces the map, Add/Update upsert. An empty list never wipes it (as with the export file).
fn apply(map: &mut BTreeMap<i32, Vet>, vets: Vec<(i32, Vet)>, replace: bool) {
    if replace && !vets.is_empty() {
        map.clear();
    }
    map.extend(vets);
}

// A field of exactly this type, else None: an Obscured struct (VALUETYPE) is never read as a number.
// ponytail: looked up per read (~150 per Uma); cache per class if UpdateAll ever hitches
fn field(obj: *mut Il2CppObject, name: &CStr, ty: Il2CppTypeEnum) -> Option<*mut FieldInfo> {
    let f = il2cpp_class_get_field_from_name(unsafe { (*obj).klass() }, name.as_ptr());
    (!f.is_null() && il2cpp_type_get_type(il2cpp_field_get_type(f)) == ty as i32).then_some(f)
}

fn int(obj: *mut Il2CppObject, name: &CStr) -> Option<i32> {
    field(obj, name, Il2CppTypeEnum_IL2CPP_TYPE_I4).map(|f| get_field_value(obj, f))
}

fn long(obj: *mut Il2CppObject, name: &CStr) -> Option<i64> {
    field(obj, name, Il2CppTypeEnum_IL2CPP_TYPE_I8).map(|f| get_field_value(obj, f))
}

fn text(obj: *mut Il2CppObject, name: &CStr) -> Option<String> {
    let s = get_field_object_value::<Il2CppString>(obj, field(obj, name, Il2CppTypeEnum_IL2CPP_TYPE_STRING)?);
    (!s.is_null()).then(|| unsafe { (*s).as_utf16str() }.to_string())
}

fn array(obj: *mut Il2CppObject, name: &CStr) -> *mut Il2CppObject {
    field(obj, name, Il2CppTypeEnum_IL2CPP_TYPE_SZARRAY).map_or(null_mut(), |f| get_field_object_value(obj, f))
}

// A rank-1 array whose element type is `ty` (I4 for i32, CLASS for objects), else empty.
pub(super) fn elems<T: Copy>(arr: *mut Il2CppObject, ty: Il2CppTypeEnum) -> Vec<T> {
    if arr.is_null() {
        return Vec::new();
    }
    let klass = unsafe { (*arr).klass() };
    let elem = if il2cpp_class_get_rank(klass) == 1 { il2cpp_class_get_element_class(klass) } else { null_mut() };
    if elem.is_null() || il2cpp_type_get_type(il2cpp_class_get_type(elem)) != ty as i32 {
        return Vec::new();
    }
    unsafe { Array::<T>::from(arr as *mut Il2CppArray).as_slice() }.to_vec()
}

fn objs(arr: *mut Il2CppObject) -> Vec<*mut Il2CppObject> {
    elems::<*mut Il2CppObject>(arr, Il2CppTypeEnum_IL2CPP_TYPE_CLASS).into_iter().filter(|p| !p.is_null()).collect()
}

fn ints(obj: *mut Il2CppObject, table: &[(&str, &CStr)]) -> Value {
    Value::Object(table.iter().map(|&(k, f)| (k.to_owned(), json!(int(obj, f)))).collect())
}

// SkillData[] (skill_id, level) or FactorInfo[] (factor_id, level)
fn levels(obj: *mut Il2CppObject, name: &CStr, id: &CStr) -> Vec<Value> {
    objs(array(obj, name)).into_iter().map(|e| json!({ "id": int(e, id), "level": int(e, c"level") })).collect()
}

fn record(tc: *mut Il2CppObject, id: i32) -> Value {
    json!({
        "id": id,
        // the chara id is card_id / 100 for every card except 9100101 and 9101101
        "card_id": int(tc, c"card_id"),
        "created": text(tc, c"create_time"),
        "rank": int(tc, c"rank"),
        "rank_score": int(tc, c"rank_score"),
        "stats": ints(tc, &STATS),
        "aptitudes": ints(tc, &APTITUDES),
        "skills": levels(tc, c"skill_array", c"skill_id"),
        "sparks": levels(tc, c"factor_info_array", c"factor_id"),
        // a parent's owner_viewer_id is read only by capture() for the overlay, never exported; user_info_summary is never read
        "parents": objs(array(tc, c"succession_chara_array")).into_iter().map(|p| json!({
            "position": int(p, c"position_id"),
            "card_id": int(p, c"card_id"),
            "sparks": levels(p, c"factor_info_array", c"factor_id")
        })).collect::<Vec<_>>(),
        "win_saddles": elems::<i32>(array(tc, c"win_saddle_id_array"), Il2CppTypeEnum_IL2CPP_TYPE_I4)
    })
}

// UseType.None (0) and WorkTrainedCharaData.MY_OWNER_VIEWER_ID (0); rentals and ghosts belong to other trainers.
// The TrainedChara.use_type values are inferred from the work model's UseType enum.
fn is_own(use_type: i32, owner: i64, viewer: i64) -> bool {
    use_type == 0 && (owner == 0 || owner == viewer)
}

// false: nothing to write. Add/Update only land on top of this account's full list (UpdateAll) from this
// session; before that, writing would replace a complete file with a partial one.
fn merge(st: &mut Store, rows: Vec<(i64, i32, Value)>, replace: bool) -> bool {
    let Some(&(viewer, _, _)) = rows.first() else { return false };
    if replace {
        st.umas.clear();
        st.viewer = Some(viewer);
    } else if st.viewer != Some(viewer) {
        return false;
    }
    st.umas.extend(rows.into_iter().map(|(_, id, v)| (id, v)));
    true
}

fn file_name(viewer: Option<i64>) -> String {
    match viewer {
        Some(v) if v > 0 => format!("trained_umas_{v}.json"),
        _ => "trained_umas.json".to_owned()
    }
}

// The game's own list only: another WorkTrainedCharaData could hold someone else's Umas.
fn is_game_list(this: *mut Il2CppObject) -> bool {
    let wdm = WorkDataManager::instance();
    !wdm.is_null() && invoke_object_method(wdm, c"get_TrainedCharaData", 0, &mut []) == Some(this)
}

// Own veterans only (same rule as the export).
fn capture(items: &[*mut Il2CppObject], replace: bool) {
    let vets: Vec<_> = items.iter().filter_map(|&tc| {
        let use_type = int(tc, c"use_type").filter(|&u| u == 0)?;
        let (id, viewer, card_id) = (int(tc, c"trained_chara_id")?, long(tc, c"viewer_id")?, int(tc, c"card_id")?);
        if !is_own(use_type, long(tc, c"owner_viewer_id")?, viewer) {
            return None;
        }
        let mut parents = [None; 2];
        for p in objs(array(tc, c"succession_chara_array")) {
            // position first: a grandparent's card and owner are never read
            let Some(slot) = int(p, c"position_id").and_then(parent_slot) else { continue };
            if let (Some(card), Some(owner)) = (int(p, c"card_id"), long(p, c"owner_viewer_id")) {
                parents[slot] = Some((card, owner));
            }
        }
        Some((id, Vet { viewer, card_id, parents }))
    }).collect();
    let mut map = lock(&PARENTS);
    apply(&mut map, vets, replace);
    GEN.fetch_add(1, Ordering::AcqRel);
    if replace {
        info!("vet parents: {} veterans captured", map.len());
    }
}

// Main thread, inside the hook: the TrainedChara objects are read before the response is dropped.
fn export(this: *mut Il2CppObject, items: Vec<*mut Il2CppObject>, replace: bool) {
    if !is_game_list(this) {
        return;
    }
    let config = Hachimi::instance().config.load();
    if config.veteran_parents_overlay {
        capture(&items, replace);
    }
    if !config.save_trained_umas {
        return;
    }
    let rows: Vec<_> = items.into_iter().filter_map(|tc| {
        // use_type first: a rental's or ghost's id fields are never read
        let use_type = int(tc, c"use_type").filter(|&u| u == 0)?;
        let (id, viewer) = (int(tc, c"trained_chara_id")?, long(tc, c"viewer_id")?);
        is_own(use_type, long(tc, c"owner_viewer_id")?, viewer).then(|| (viewer, id, record(tc, id)))
    }).collect();
    if !merge(&mut lock(&STORE), rows, replace) {
        return;
    }
    if !PENDING.swap(true, Ordering::AcqRel) {
        std::thread::spawn(write);
    }
}

// Worker thread. One writer at a time (two File::create on one path would interleave). PENDING is cleared
// under STORE before the clone, so any later merge spawns a fresh writer; a burst becomes one write.
fn write() {
    let _one = lock(&WRITE);
    let (viewer, umas) = {
        let st = lock(&STORE);
        PENDING.store(false, Ordering::Release);
        let Some(viewer) = st.viewer else { return }; // base forgotten while off: nothing current to write
        (viewer, st.umas.values().cloned().collect::<Vec<_>>())
    };
    let path = Hachimi::instance().game.data_dir.join(file_name(Some(viewer)));
    match utils::write_json_file(&umas, &path) {
        Ok(()) => info!("trained uma export: {} Umas -> {}", umas.len(), path.display()),
        Err(e) => warn!("trained uma export: write failed: {}", e)
    }
}

// The game may call Update from inside UpdateAll / AddTrainedCharaArray: only the outermost call exports.
// While the export is off its base list is forgotten, so after turning it back on deltas wait for a fresh
// UpdateAll instead of landing on a list that missed changes. Likewise the parents map is emptied while the
// overlay is off (no lender ids or another account's veterans kept); a veteran it misses gets no overlay.
// ponytail: emptied on the next list call after the overlay goes off, not at the toggle itself
fn outermost_enabled(orig: impl FnOnce()) -> bool {
    let outer = !BUSY.swap(true, Ordering::AcqRel);
    orig();
    if !outer {
        return false;
    }
    BUSY.store(false, Ordering::Release);
    let config = Hachimi::instance().config.load();
    if !config.save_trained_umas {
        lock(&STORE).viewer = None;
    }
    if !config.veteran_parents_overlay {
        lock(&PARENTS).clear();
    }
    config.save_trained_umas || config.veteran_parents_overlay
}

type ArrayFn = extern "C" fn(this: *mut Il2CppObject, arr: *mut Il2CppObject);
extern "C" fn WorkTrainedCharaData_UpdateAll(this: *mut Il2CppObject, arr: *mut Il2CppObject) {
    if outermost_enabled(|| get_orig_fn!(WorkTrainedCharaData_UpdateAll, ArrayFn)(this, arr)) {
        export(this, objs(arr), true);
    }
}

extern "C" fn WorkTrainedCharaData_AddTrainedCharaArray(this: *mut Il2CppObject, arr: *mut Il2CppObject) {
    if outermost_enabled(|| get_orig_fn!(WorkTrainedCharaData_AddTrainedCharaArray, ArrayFn)(this, arr)) {
        export(this, objs(arr), false);
    }
}

type UpdateFn = extern "C" fn(this: *mut Il2CppObject, chara: *mut Il2CppObject);
extern "C" fn WorkTrainedCharaData_Update(this: *mut Il2CppObject, chara: *mut Il2CppObject) {
    if outermost_enabled(|| get_orig_fn!(WorkTrainedCharaData_Update, UpdateFn)(this, chara)) && !chara.is_null() {
        export(this, vec![chara], false);
    }
}

pub fn init(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, WorkTrainedCharaData);
    // by parameter type: another overload with one argument is never hooked
    let UpdateAll_addr = get_method_overload_addr(WorkTrainedCharaData, "UpdateAll", &[Il2CppTypeEnum_IL2CPP_TYPE_SZARRAY]);
    let AddTrainedCharaArray_addr = get_method_overload_addr(WorkTrainedCharaData, "AddTrainedCharaArray", &[Il2CppTypeEnum_IL2CPP_TYPE_SZARRAY]);
    let Update_addr = get_method_overload_addr(WorkTrainedCharaData, "Update", &[Il2CppTypeEnum_IL2CPP_TYPE_CLASS]);
    new_hook!(UpdateAll_addr, WorkTrainedCharaData_UpdateAll);
    new_hook!(AddTrainedCharaArray_addr, WorkTrainedCharaData_AddTrainedCharaArray);
    new_hook!(Update_addr, WorkTrainedCharaData_Update);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trained_uma_export_merge() {
        assert!(is_own(0, 0, 7) && is_own(0, 7, 7));
        assert!(!is_own(1, 0, 7) && !is_own(2, 0, 7) && !is_own(0, 9, 7), "rental, ghost, other owner");

        let row = |v: i64, id: i32| (v, id, json!(id));
        let mut st = Store::default();
        assert!(!merge(&mut st, vec![row(7, 3)], false) && st.viewer.is_none() && st.umas.is_empty(), "a delta before the full list");
        assert!(!merge(&mut st, vec![], true), "an empty list never wipes the file");
        assert!(merge(&mut st, vec![row(7, 1), row(7, 2)], true));
        assert!(merge(&mut st, vec![row(7, 3)], false));
        assert_eq!(st.umas.keys().copied().collect::<Vec<_>>(), [1, 2, 3]);
        assert!(merge(&mut st, vec![row(7, 2)], true));
        assert_eq!(st.umas.len(), 1, "UpdateAll replaces the list");
        assert!(!merge(&mut st, vec![row(8, 5)], false));
        assert_eq!((st.viewer, st.umas.len()), (Some(7), 1), "another account's delta is ignored");
        assert!(merge(&mut st, vec![row(8, 5)], true));
        assert_eq!((st.viewer, st.umas.len()), (Some(8), 1));

        let vet = |card_id| Vet { viewer: 7, card_id, parents: [None; 2] };
        let cards = |m: &BTreeMap<i32, Vet>| m.iter().map(|(&k, v)| (k, v.card_id)).collect::<Vec<_>>();
        let mut map = BTreeMap::new();
        apply(&mut map, vec![(1, vet(10)), (2, vet(20))], true);
        apply(&mut map, vec![(2, vet(21)), (3, vet(30))], false);
        assert_eq!(cards(&map), [(1, 10), (2, 21), (3, 30)], "Add/Update upsert");
        apply(&mut map, vec![], true);
        assert_eq!(map.len(), 3, "an empty UpdateAll never wipes the parents");
        apply(&mut map, vec![(4, vet(40))], true);
        assert_eq!(cards(&map), [(4, 40)], "UpdateAll replaces the parents");

        assert_eq!(file_name(Some(8)), "trained_umas_8.json");
        assert_eq!(file_name(None), "trained_umas.json");
    }
}
