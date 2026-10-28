//! Legacy (inheritance) step at career start: adds up the affinity points of the chosen
//! parents from master.mdb so the GUI can show them. Read-only, local, no server traffic.
use std::{ffi::CStr, sync::{atomic::{AtomicBool, Ordering}, Mutex}};
use crate::{
    core::Hachimi,
    il2cpp::{
        api::{il2cpp_class_get_element_class, il2cpp_class_get_rank, il2cpp_class_is_valuetype},
        ext::Il2CppObjectExt,
        sql,
        symbols::{get_field_from_name, get_field_object_value, get_field_value, get_method_addr, invoke_object_method, unbox, Array},
        types::*
    }
};
use super::DialogManager;

#[derive(Clone, Copy, Default)]
pub struct Affinity {
    /// trainee + parent, plus trainee + parent + each of that parent's parents
    pub parents: [i32; 2],
    /// parent 1 + parent 2
    pub pair: i32,
    /// G1 wins each parent shares with its own parents (+1 each)
    pub wins: i32,
    pub total: i32,
}

static OPEN: AtomicBool = AtomicBool::new(false);
static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);
static AFFINITY: Mutex<Option<Affinity>> = Mutex::new(None);

/// GUI thread: the numbers to draw, or None.
pub fn overlay() -> Option<Affinity> {
    if DIALOG_OPEN.load(Ordering::Acquire) { return None; }
    *AFFINITY.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn clear() {
    OPEN.store(false, Ordering::Release);
    *AFFINITY.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Main thread, every frame. GetForeFrontDialog is a runtime_invoke, so never call it from the GUI thread.
pub fn refresh_dialog() {
    if !OPEN.load(Ordering::Acquire) { return; }
    DIALOG_OPEN.store(DialogManager::GetForeFrontDialog().is_some_and(|d| !d.is_null()), Ordering::Release);
}

fn obj_field(obj: *mut Il2CppObject, name: &CStr) -> Option<*mut Il2CppObject> {
    let field = get_field_from_name(unsafe { (*obj).klass() }, name);
    if field.is_null() { return None; }
    let value = get_field_object_value::<Il2CppObject>(obj, field);
    (!value.is_null()).then_some(value)
}

// looked up by name on the runtime class, so a wrong guess about a type gives None, not a crash
fn call_obj(obj: *mut Il2CppObject, name: &CStr) -> Option<*mut Il2CppObject> {
    invoke_object_method(obj, name, 0, &mut []).filter(|r| !r.is_null())
}

fn call_i32(obj: *mut Il2CppObject, name: &CStr) -> Option<i32> {
    call_obj(obj, name).map(|r| unsafe { unbox::<i32>(r) })
}

fn field_i32(obj: *mut Il2CppObject, name: &CStr) -> Option<i32> {
    let field = get_field_from_name(unsafe { (*obj).klass() }, name);
    (!field.is_null()).then(|| get_field_value::<i32>(obj, field))
}

/// Global chara ids are 1001..=1999 (succession_relation_member); anything else is a misread, never queried.
fn chara_id(id: Option<i32>, what: &str) -> Option<i32> {
    let id = id?;
    if (1001..=1999).contains(&id) { return Some(id); }
    warn!("affinity: unreadable {what} chara id {id}");
    None
}

// _winSaddleIdArray is ObscuredInt[]; get_WinSaddleArray maps it to master SingleModeWinsSaddle rows (plain int Id).
// The element type is inferred from metadata, so it is checked: a value-type element or one without
// an Id field gives an empty list (and at most one field warning), never a read of struct bytes.
fn win_saddles(obj: *mut Il2CppObject) -> Vec<i32> {
    let Some(arr) = call_obj(obj, c"get_WinSaddleArray") else { return Vec::new() };
    let klass = unsafe { (*arr).klass() };
    if il2cpp_class_get_rank(klass) != 1 { return Vec::new(); }
    let elem = il2cpp_class_get_element_class(klass);
    if elem.is_null() || il2cpp_class_is_valuetype(elem) { return Vec::new(); }
    let id = get_field_from_name(elem, c"Id");
    if id.is_null() { return Vec::new(); }
    unsafe { Array::<*mut Il2CppObject>::from(arr as *mut Il2CppArray).as_slice() }
        .iter().filter(|e| !e.is_null()).map(|&e| get_field_value::<i32>(e, id)).collect()
}

/// G1 saddles a parent shares with one of its own parents.
fn shared_g1(a: &[i32], b: &[i32], g1: &[i32]) -> i32 {
    a.iter().filter(|s| g1.contains(s) && b.contains(s)).count() as i32
}

fn rel(ids: &[i32]) -> i32 {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup(); // a grandparent can be the trainee; the game intersects sets, so a repeat adds nothing
    sql::get_relation_point(&ids)
}

fn compute(this: *mut Il2CppObject) -> Option<Affinity> {
    let entry = obj_field(this, c"_partsSuccessionSelect").and_then(|p| obj_field(p, c"_entryInfo"))?;
    let trainee = chara_id(call_i32(entry, c"GetCharaId"), "trainee")?; // EntryInfo::GetCharaId is a plain Int32
    let g1 = sql::get_g1_saddle_ids();
    let mut a = Affinity::default();
    let mut ids = [None; 2];
    let mut gps = [[None; 2]; 2];
    let mut saddle_n = [0; 2];
    for (i, name) in [c"SuccessionTrainedChara_First", c"SuccessionTrainedChara_Second"].into_iter().enumerate() {
        let Some(parent) = obj_field(entry, name) else { continue }; // empty slot
        // TrainedCharaData::get_CharaId returns ObscuredInt; the master card row's CharaId is a plain int
        let pid = call_obj(parent, c"get_MasterCardData").and_then(|c| field_i32(c, c"CharaId"));
        let Some(pid) = chara_id(pid, "parent") else { continue };
        ids[i] = Some(pid);
        let saddles = win_saddles(parent);
        saddle_n[i] = saddles.len();
        a.parents[i] = rel(&[trainee, pid]);
        for (j, getter) in [c"get_SuccessionChara1", c"get_SuccessionChara2"].into_iter().enumerate() {
            let Some(gp) = call_obj(parent, getter) else { continue };
            // SuccessionCharaData::get_CharaId is a plain Int32
            let Some(gid) = chara_id(call_i32(gp, c"get_CharaId"), "grandparent") else { continue };
            gps[i][j] = Some(gid);
            a.parents[i] += rel(&[trainee, pid, gid]);
            a.wins += shared_g1(&saddles, &win_saddles(gp), &g1);
        }
    }
    if let [Some(p1), Some(p2)] = ids {
        a.pair = rel(&[p1, p2]);
    }
    a.total = a.parents[0] + a.parents[1] + a.pair + a.wins;
    info!("affinity: trainee={trainee} parents={ids:?} grandparents={gps:?} saddles={saddle_n:?} -> p1={} p2={} pair={} wins={} total={}",
        a.parents[0], a.parents[1], a.pair, a.wins, a.total);
    Some(a)
}

fn recompute(this: *mut Il2CppObject) {
    let a = compute(this);
    *AFFINITY.lock().unwrap_or_else(|e| e.into_inner()) = a;
}

type ShowFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn SuccessionSelect_Show(this: *mut Il2CppObject) {
    get_orig_fn!(SuccessionSelect_Show, ShowFn)(this);
    if Hachimi::instance().config.load().affinity_overlay {
        OPEN.store(true, Ordering::Release);
        recompute(this);
    }
}

// runs after every pick / auto-select / reset (INFERRED from the name)
type UpdateNextButtonFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn SuccessionSelect_UpdateNextButton(this: *mut Il2CppObject) {
    get_orig_fn!(SuccessionSelect_UpdateNextButton, UpdateNextButtonFn)(this);
    if OPEN.load(Ordering::Acquire) {
        recompute(this);
    }
}

// Hide(1): the argument's type isn't in the dump; it is passed through as one pointer-sized register
type HideFn = extern "C" fn(this: *mut Il2CppObject, arg: *mut Il2CppObject);
extern "C" fn SuccessionSelect_Hide(this: *mut Il2CppObject, arg: *mut Il2CppObject) {
    get_orig_fn!(SuccessionSelect_Hide, HideFn)(this, arg);
    clear();
}

pub fn init(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, SingleModeStartStepSuccessionSelect);

    let Show_addr = get_method_addr(SingleModeStartStepSuccessionSelect, c"Show", 0);
    let UpdateNextButton_addr = get_method_addr(SingleModeStartStepSuccessionSelect, c"UpdateNextButton", 0);
    let Hide_addr = get_method_addr(SingleModeStartStepSuccessionSelect, c"Hide", 1);

    new_hook!(Show_addr, SuccessionSelect_Show);
    new_hook!(UpdateNextButton_addr, SuccessionSelect_UpdateNextButton);
    new_hook!(Hide_addr, SuccessionSelect_Hide);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_shared_g1() {
        let g1 = [10, 11, 12];
        assert_eq!(shared_g1(&[10, 11, 20], &[11, 10, 20], &g1), 2, "non-G1 20 not counted");
        assert_eq!(shared_g1(&[10], &[], &g1), 0);
        assert_eq!(shared_g1(&[], &[10], &g1), 0);
    }

    #[test]
    fn affinity_chara_id_guard() {
        assert_eq!(chara_id(Some(1001), "t"), Some(1001));
        assert_eq!(chara_id(Some(-123456789), "t"), None, "crypto-key-like value rejected");
        assert_eq!(chara_id(Some(100101), "t"), None, "card id rejected");
        assert_eq!(chara_id(None, "t"), None);
    }
}
