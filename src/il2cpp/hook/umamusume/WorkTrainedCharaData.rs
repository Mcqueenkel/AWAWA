// Trained Uma export: your own veterans to <data dir>/trained_umas_<viewer id>.json, rewritten whenever the
// game loads or updates its veteran list. Read-only, local file only. Everything comes from the plain server
// models (Gallop.TrainedChara) the game hands to WorkTrainedCharaData; its work copies keep sparks, create
// time and chara id as Obscured*, which are never read here.
use std::{collections::BTreeMap, ffi::CStr, ptr::null_mut, sync::{atomic::{AtomicBool, Ordering}, Mutex}};

use serde_json::{json, Value};

use crate::{
    core::{utils, Hachimi},
    il2cpp::{
        api::{il2cpp_class_get_element_class, il2cpp_class_get_field_from_name, il2cpp_class_get_rank, il2cpp_class_get_type, il2cpp_field_get_type, il2cpp_type_get_type},
        ext::{Il2CppObjectExt, Il2CppStringExt},
        symbols::{get_field_object_value, get_field_value, get_method_addr, invoke_object_method, Array},
        types::*
    }
};
use super::{SuperSkip::lock, WorkDataManager};

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
fn elems<T: Copy>(arr: *mut Il2CppObject, ty: Il2CppTypeEnum) -> Vec<T> {
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
        // owner_viewer_id / user_info_summary name another trainer for a borrowed parent: never read
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

fn enabled() -> bool {
    Hachimi::instance().config.load().save_trained_umas
}

// Main thread, inside the hook: the TrainedChara objects are read before the response is dropped.
fn export(this: *mut Il2CppObject, items: Vec<*mut Il2CppObject>, replace: bool) {
    if !is_game_list(this) {
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
// While the setting is off the base list is forgotten, so after turning it back on deltas wait for a fresh
// UpdateAll instead of landing on a list that missed changes.
fn outermost_enabled(orig: impl FnOnce()) -> bool {
    let outer = !BUSY.swap(true, Ordering::AcqRel);
    orig();
    if !outer {
        return false;
    }
    BUSY.store(false, Ordering::Release);
    if !enabled() {
        lock(&STORE).viewer = None;
        return false;
    }
    true
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
    let UpdateAll_addr = get_method_addr(WorkTrainedCharaData, c"UpdateAll", 1);
    let AddTrainedCharaArray_addr = get_method_addr(WorkTrainedCharaData, c"AddTrainedCharaArray", 1);
    let Update_addr = get_method_addr(WorkTrainedCharaData, c"Update", 1);
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

        assert_eq!(file_name(Some(8)), "trained_umas_8.json");
        assert_eq!(file_name(None), "trained_umas.json");
    }
}
