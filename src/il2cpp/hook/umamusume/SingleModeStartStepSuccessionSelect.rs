//! Legacy (inheritance) step at career start: adds up the affinity points of the chosen
//! parents from master.mdb so the GUI can show them. Read-only, local, no server traffic.
use std::{ffi::{c_char, CStr}, ptr::null_mut, sync::{atomic::{AtomicBool, Ordering}, Mutex}};
use crate::{
    core::{game::Region, Hachimi},
    il2cpp::{
        api::{il2cpp_class_from_type, il2cpp_class_get_element_class, il2cpp_class_get_methods, il2cpp_class_get_name,
            il2cpp_class_get_parent, il2cpp_class_get_rank, il2cpp_class_is_valuetype, il2cpp_field_get_name, il2cpp_field_get_type,
            il2cpp_free, il2cpp_method_get_name, il2cpp_method_get_param_count, il2cpp_type_get_name, il2cpp_type_get_type},
        ext::Il2CppObjectExt,
        sql,
        symbols::{get_field_from_name, get_field_object_value, get_field_value, get_method_addr, invoke_object_method, Array, FieldsIter},
        types::*
    }
};
use super::{DialogManager, SuperSkip::invoke_i32};

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

fn is_i4(field: *mut FieldInfo) -> bool {
    !field.is_null() && il2cpp_type_get_type(il2cpp_field_get_type(field)) == Il2CppTypeEnum_IL2CPP_TYPE_I4 as i32
}

fn field_i32(obj: *mut Il2CppObject, name: &CStr) -> Option<i32> {
    let field = get_field_from_name(unsafe { (*obj).klass() }, name);
    is_i4(field).then(|| get_field_value::<i32>(obj, field))
}

/// Global chara ids are 1001..=1999 (succession_relation_member); anything else is a misread, never queried.
/// The value itself is never logged.
fn chara_id(id: Option<i32>, what: &str) -> Option<i32> {
    let id = id?;
    if (1001..=1999).contains(&id) { return Some(id); }
    warn!("affinity: unreadable {what} chara id");
    None
}

// _winSaddleIdArray is ObscuredInt[]; get_WinSaddleArray maps it to master SingleModeWinsSaddle rows (plain int Id).
// The element type is inferred from metadata, so it is checked: a value-type element or one without
// an Int32 Id field gives an empty list (and at most one field warning), never a read of struct bytes.
fn win_saddles(obj: *mut Il2CppObject) -> Vec<i32> {
    let Some(arr) = call_obj(obj, c"get_WinSaddleArray") else { return Vec::new() };
    let klass = unsafe { (*arr).klass() };
    if il2cpp_class_get_rank(klass) != 1 { return Vec::new(); }
    let elem = il2cpp_class_get_element_class(klass);
    if elem.is_null() || il2cpp_class_is_valuetype(elem) { return Vec::new(); }
    let id = get_field_from_name(elem, c"Id");
    if !is_i4(id) { return Vec::new(); }
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

// ponytail: JP diagnostic, delete once the JP entry-info chain is known. Member names and types only, never values.
static DUMPED: AtomicBool = AtomicBool::new(false);

fn cstr(p: *const c_char) -> String {
    if p.is_null() { return "?".to_owned(); }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn type_name(ty: *const Il2CppType) -> String {
    let p = il2cpp_type_get_name(ty);
    let s = cstr(p);
    if !p.is_null() { il2cpp_free(p.cast()); }
    s
}

// the class and its Gallop parents (Unity bases skipped); returns the field types for follow-up dumps
fn dump_class(what: &str, mut class: *mut Il2CppClass) -> Vec<*const Il2CppType> {
    let mut types = Vec::new();
    for _ in 0..4 {
        if class.is_null() { break; }
        let name = cstr(il2cpp_class_get_name(class));
        if ["MonoBehaviour", "Behaviour", "Component", "Object"].contains(&name.as_str()) { break; }
        let fields: Vec<String> = FieldsIter::new(class).map(|f| {
            let ty = il2cpp_field_get_type(f);
            types.push(ty);
            format!("{}: {}", cstr(il2cpp_field_get_name(f)), type_name(ty))
        }).collect();
        let mut iter = null_mut();
        let mut methods = Vec::new();
        loop {
            let m = il2cpp_class_get_methods(class, &mut iter);
            if m.is_null() { break; }
            methods.push(format!("{}({})", cstr(il2cpp_method_get_name(m)), il2cpp_method_get_param_count(m)));
        }
        info!("affinity dump {what} {name}: fields [{}] methods [{}]", fields.join(", "), methods.join(", "));
        class = il2cpp_class_get_parent(class);
    }
    types
}

fn dump(this: *mut Il2CppObject, parts: Option<*mut Il2CppObject>) {
    dump_class("step", unsafe { (*this).klass() });
    let Some(parts) = parts else { return };
    for ty in dump_class("parts", unsafe { (*parts).klass() }) {
        if type_name(ty).contains("Entry") {
            dump_class("entry", il2cpp_class_from_type(ty));
        }
    }
}

fn compute(this: *mut Il2CppObject) -> Option<Affinity> {
    let parts = obj_field(this, c"_partsSuccessionSelect");
    let Some(entry) = parts.and_then(|p| obj_field(p, c"_entryInfo")) else {
        if Hachimi::instance().game.region != Region::Global && !DUMPED.swap(true, Ordering::Relaxed) {
            dump(this, parts);
        }
        return None;
    };
    let trainee = chara_id(invoke_i32(entry, c"GetCharaId"), "trainee")?; // EntryInfo::GetCharaId is a plain Int32
    let g1 = sql::get_g1_saddle_ids();
    let jp = Hachimi::instance().game.region == Region::Japan;
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
            let Some(gid) = chara_id(invoke_i32(gp, c"get_CharaId"), "grandparent") else { continue };
            gps[i][j] = Some(gid);
            a.parents[i] += rel(&[trainee, pid, gid]);
            let gp_saddles = win_saddles(gp);
            // JP: one race can have several saddle ids, matched by relation group; else (or on failure) exact ids
            let grouped = if jp { sql::get_shared_g1_groups(&saddles, &gp_saddles) } else { None };
            a.wins += grouped.unwrap_or_else(|| shared_g1(&saddles, &gp_saddles, &g1));
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
    // all or none: without Hide the overlay would stay up after the screen closes
    if [Show_addr, UpdateNextButton_addr, Hide_addr].contains(&0) {
        warn!("affinity: SingleModeStartStepSuccessionSelect methods missing, affinity overlay unavailable");
        return;
    }

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
