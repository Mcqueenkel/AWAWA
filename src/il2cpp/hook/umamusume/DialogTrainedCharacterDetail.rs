// Veteran parents overlay: on the detail dialog of one of your own veterans, lists its two parents and
// whether each was yours or borrowed (with the lender's Trainer ID). Read-only. The parents come from the
// plain TrainedChara capture in WorkTrainedCharaData; lender ids are shown on screen only, never logged.
use std::{collections::BTreeMap, ffi::CStr, ptr::null_mut, sync::{Mutex, OnceLock}};
use rust_i18n::t;

use crate::{
    core::Hachimi,
    il2cpp::{
        api::{il2cpp_class_from_type, il2cpp_class_get_field_from_name, il2cpp_class_is_assignable_from, il2cpp_class_is_subclass_of, il2cpp_field_get_type, il2cpp_type_get_type},
        ext::Il2CppObjectExt,
        sql,
        symbols::{get_class, get_field_object_value, get_field_value, get_method, invoke_object_method, GCHandle},
        types::*
    }
};
use super::{DialogManager, SuperSkip::{invoke_get, lock}, WorkTrainedCharaData};

// FieldInfo / class pointers as usize; set once by init when everything resolved
struct Fields { dialog: usize, header: usize, inner_own: usize, id: usize, card: usize }
static FIELDS: OnceLock<Fields> = OnceLock::new();

// ponytail: one handle; a detail dialog opened over another takes it, and the one below stays bare after the
// top one closes (fails safe). A Vec of handles if stacked detail dialogs ever matter.
static DIALOG: Mutex<Option<GCHandle>> = Mutex::new(None); // the open DialogTrainedCharacterDetail

// Main thread only: the (trained chara id, card id) and capture generation the lines were built for, and
// card names by card id (one master.mdb query per card per session).
struct State { key: Option<(i32, i32)>, gen: u32, lines: Option<(i32, Vec<String>)>, up: bool, names: BTreeMap<i32, String> }
static STATE: Mutex<State> = Mutex::new(State { key: None, gen: 0, lines: None, up: false, names: BTreeMap::new() });
static LINES: Mutex<Option<(i32, Vec<String>)>> = Mutex::new(None); // GUI: (veteran id, lines) while shown

pub fn showing() -> bool {
    lock(&LINES).is_some()
}

pub fn overlay() -> Option<(i32, Vec<String>)> {
    lock(&LINES).clone()
}

// SuccessionChara.position_id: 10 = parent 1, 20 = parent 2 (11/12/21/22 grandparents, 1 self)
pub(super) fn parent_slot(position: i32) -> Option<usize> {
    match position {
        10 => Some(0),
        20 => Some(1),
        _ => None
    }
}

// 9 digits zero-padded, grouped in threes from the right
fn trainer_id(v: i64) -> String {
    let s = format!("{v:09}");
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

// Some(None): the player's own (0 is WorkTrainedCharaData.MY_OWNER_VIEWER_ID); Some(Some(id)): borrowed;
// None: a negative owner id, unknown
fn lender(owner: i64, viewer: i64) -> Option<Option<String>> {
    (owner >= 0).then(|| (owner != 0 && owner != viewer).then(|| trainer_id(owner)))
}

// Main thread (master.mdb goes through the game's SQLite; STATE, which holds `names`, is never locked by the GUI).
fn build(id: i32, card: i32, names: &mut BTreeMap<i32, String>) -> Option<Vec<String>> {
    if id <= 0 {
        return None; // the own header was never set up: someone else's or a mob's Uma
    }
    let Some(vet) = WorkTrainedCharaData::vet(id) else {
        info!("vet parents: veteran {id}: not captured yet");
        return None;
    };
    // ponytail: 0 = the dialog never set _cardId (seen on Global: every open hid the box); the active own
    // panel's id is then the only check. Card ids are master data, fine to log.
    if card != 0 && vet.card_id != card {
        info!("vet parents: veteran {id}: card mismatch (dialog {card}, veteran {}), hidden", vet.card_id);
        return None;
    }
    if card == 0 {
        info!("vet parents: veteran {id}: dialog card not set, id check only");
    }
    let mut kinds = ["-"; 2];
    let lines = vet.parents.iter().zip([t!("affinity_overlay.parent_1"), t!("affinity_overlay.parent_2")]).enumerate()
        .map(|(i, (p, label))| match *p {
            None => format!("{label}: {}", t!("vet_parents_overlay.missing")),
            Some((card, owner)) => {
                let name = names.entry(card).or_insert_with(|| sql::get_master_text(4, card).unwrap_or_else(|| format!("#{card}")));
                let whose = match lender(owner, vet.viewer) {
                    None => { kinds[i] = "unknown"; t!("vet_parents_overlay.missing") }
                    Some(None) => { kinds[i] = "yours"; t!("vet_parents_overlay.yours") }
                    Some(Some(tid)) => { kinds[i] = "borrowed"; t!("vet_parents_overlay.borrowed", id = tid) }
                };
                format!("{label}: {name}, {whose}")
            }
        }).collect();
    info!("vet parents: veteran {id}: parent 1 {}, parent 2 {}", kinds[0], kinds[1]);
    Some(lines)
}

// (DialogTrainedCharacterDetail._headerTitle._innerOwn._trainedCharaId, DialogCharacterDetailBase._cardId)
// _innerOwn keeps its id when the dialog switches to another player's Uma: only an active panel counts.
// Runtime invoke: main thread only.
fn shown(dialog: *mut Il2CppObject, f: &Fields) -> Option<(i32, i32)> {
    let header = get_field_object_value::<Il2CppObject>(dialog, f.header as _);
    if header.is_null() {
        return None;
    }
    let own = get_field_object_value::<Il2CppObject>(header, f.inner_own as _);
    if own.is_null() || invoke_get::<bool>(own, c"get_isActiveAndEnabled") != Some(true) {
        return None;
    }
    Some((get_field_value(own, f.id as _), get_field_value(dialog, f.card as _)))
}

// GetForeFrontDialog / GetDialog are runtime invokes: main thread only.
fn in_front(dialog: *mut Il2CppObject) -> bool {
    DialogManager::GetForeFrontDialog()
        .is_some_and(|front| !front.is_null() && invoke_object_method(dialog, c"GetDialog", 0, &mut []) == Some(front))
}

// Main thread, every frame.
pub fn tick() {
    let Some(f) = FIELDS.get() else { return };
    let dialog = if Hachimi::instance().config.load().veteran_parents_overlay {
        lock(&DIALOG).as_ref().map_or(null_mut(), |h| h.target())
    } else {
        null_mut()
    };
    let key = if dialog.is_null() { None } else { shown(dialog, f) };
    let gen = WorkTrainedCharaData::generation(); // a capture after the dialog opened fills it in
    let mut guard = lock(&STATE);
    let st = &mut *guard;
    let changed = st.key != key || st.gen != gen;
    if changed {
        st.key = key;
        st.gen = gen;
        st.lines = key.and_then(|(id, card)| build(id, card, &mut st.names).map(|l| (id, l)));
    }
    let up = st.lines.is_some() && in_front(dialog);
    if changed || up != st.up {
        st.up = up;
        *lock(&LINES) = if up { st.lines.clone() } else { None };
    }
}

type SetupFn = extern "C" fn(this: *mut Il2CppObject, parameter: *mut Il2CppObject);
extern "C" fn DialogTrainedCharacterDetail_Setup(this: *mut Il2CppObject, parameter: *mut Il2CppObject) {
    get_orig_fn!(DialogTrainedCharacterDetail_Setup, SetupFn)(this, parameter);
    // a base-class Setup found by name would serve other detail dialogs: bind this type only
    let Some(f) = FIELDS.get() else { return };
    if !this.is_null() && il2cpp_class_is_assignable_from(f.dialog as _, unsafe { (*this).klass() }) {
        *lock(&DIALOG) = Some(GCHandle::new_weak_ref(this, false));
    }
}

type OnDestroyFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn DialogCharacterDetailBase_OnDestroy(this: *mut Il2CppObject) {
    {
        let mut d = lock(&DIALOG);
        if d.as_ref().is_some_and(|h| h.target() == this) {
            *d = None;
        }
    }
    get_orig_fn!(DialogCharacterDetailBase_OnDestroy, OnDestroyFn)(this);
}

// `name` on `class`, typed exactly as the class `of`, or as a plain Int32 when `of` is None (never an Obscured struct).
fn field(class: *mut Il2CppClass, name: &CStr, of: Option<*mut Il2CppClass>) -> usize {
    if class.is_null() {
        return 0;
    }
    let f = il2cpp_class_get_field_from_name(class, name.as_ptr());
    if f.is_null() {
        return 0;
    }
    let ty = il2cpp_field_get_type(f);
    let ok = match of {
        Some(c) => !c.is_null() && il2cpp_class_from_type(ty) == c,
        None => il2cpp_type_get_type(ty) == Il2CppTypeEnum_IL2CPP_TYPE_I4 as i32
    };
    if ok { f as usize } else { 0 }
}

fn method_addr(class: *mut Il2CppClass, name: &CStr, argc: i32) -> usize {
    if class.is_null() {
        return 0;
    }
    get_method(class, name, argc).map_or(0, |m| unsafe { (*m).methodPointer })
}

// ponytail: if _innerOwn._trainedCharaId goes missing, PartsTrainedCharacterDetailHeaderTitle::SetupForOwn(2)
// (parameter.Own.TrainedCharaId, public Int32) is the fallback id source; not built while the chain exists
pub fn init(umamusume: *const Il2CppImage) {
    let class = |name: &CStr| get_class(umamusume, c"Gallop", name).unwrap_or(null_mut());
    let dialog = class(c"DialogTrainedCharacterDetail");
    let base = class(c"DialogCharacterDetailBase");
    let header = class(c"PartsTrainedCharacterDetailHeaderTitle");
    let own = class(c"PartsTrainedCharacterDetailInnerOwn");
    let f = Fields {
        dialog: dialog as usize,
        header: field(dialog, c"_headerTitle", Some(header)),
        inner_own: field(header, c"_innerOwn", Some(own)),
        id: field(own, c"_trainedCharaId", None),
        card: field(base, c"_cardId", None)
    };
    let Setup_addr = method_addr(dialog, c"Setup", 1);
    let OnDestroy_addr = method_addr(base, c"OnDestroy", 0);
    let get_dialog = method_addr(class(c"DialogInnerBase"), c"GetDialog", 0);
    // all or none: without the front check the overlay could sit over another dialog. _cardId and OnDestroy
    // come from the base, so the dialog must derive from it (non-null: both checked by the zeros above).
    if [f.dialog, f.header, f.inner_own, f.id, f.card, Setup_addr, OnDestroy_addr, get_dialog].contains(&0)
        || !il2cpp_class_is_subclass_of(dialog, base, false)
        || !DialogManager::fore_front_dialog_available() {
        warn!("vet parents: dialog members missing, overlay unavailable");
        return;
    }
    let _ = FIELDS.set(f);
    new_hook!(Setup_addr, DialogTrainedCharacterDetail_Setup);
    new_hook!(OnDestroy_addr, DialogCharacterDetailBase_OnDestroy);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vet_parents_helpers() {
        assert_eq!(trainer_id(123456789), "123 456 789");
        assert_eq!(trainer_id(1234), "000 001 234");
        assert_eq!(trainer_id(1234567890), "1 234 567 890");
        assert_eq!(lender(0, 7), Some(None), "MY_OWNER_VIEWER_ID");
        assert_eq!(lender(7, 7), Some(None), "the veteran's own viewer");
        assert_eq!(lender(1234, 7), Some(Some("000 001 234".to_owned())));
        assert_eq!(lender(-1, 7), None, "a negative owner is unknown, never shown as an id");
        assert_eq!((parent_slot(10), parent_slot(20)), (Some(0), Some(1)));
        assert!([1, 11, 12, 21, 22].iter().all(|&p| parent_slot(p).is_none()), "self and grandparents");
    }
}
