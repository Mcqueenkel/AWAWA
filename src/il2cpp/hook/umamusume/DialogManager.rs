use std::ptr::null_mut;
use crate::{il2cpp::{api::il2cpp_runtime_invoke, ext::StringExt, symbols::{get_method, get_method_addr, SingletonLike}, types::*}};
use super::{DialogCommon::{Data, FormType}, TextId};

static mut CLASS: *mut Il2CppClass = null_mut();
pub fn class() -> *mut Il2CppClass {
    unsafe { CLASS }
}

pub fn instance() -> *mut Il2CppObject {
    let Some(singleton) = SingletonLike::new(class()) else {
        return null_mut();
    };
    singleton.instance()
}

static mut PUSHDIALOG_ADDR: usize = 0;
impl_addr_wrapper_fn!(PushDialog, PUSHDIALOG_ADDR, *mut Il2CppObject, data: *mut Il2CppObject);

// public static DialogCommon PushSystemDialog(Data dialogData, Boolean isEnableOutsideClick) { }
static mut PUSHSYSTEMDIALOG_ADDR: usize = 0;
impl_addr_wrapper_fn!(PushSystemDialog, PUSHSYSTEMDIALOG_ADDR, *mut Il2CppObject, dialogData: *mut Il2CppObject, isEnableOutsideClick: bool);

// public static DialogCommonBase GetForeFrontDialog() { }
static mut GETFOREFRONTDIALOG_METHOD: *const MethodInfo = std::ptr::null();
pub fn fore_front_dialog_available() -> bool {
    unsafe { !GETFOREFRONTDIALOG_METHOD.is_null() }
}

/// Some(front-most open dialog, or null when none is open). None when the method is missing
/// or the game threw, so callers can't mistake "unknown" for "no dialog".
pub fn GetForeFrontDialog() -> Option<*mut Il2CppObject> {
    let method = unsafe { GETFOREFRONTDIALOG_METHOD };
    if method.is_null() {
        return None;
    }
    let mut exc = null_mut();
    let res = il2cpp_runtime_invoke(method, null_mut(), null_mut(), &mut exc);
    exc.is_null().then_some(res)
}

pub fn single_button_message(title: &str, message: &str, typ: FormType) {
    let dialog_data = Data::new();
    Data::SetSimpleOneButtonMessage(
        dialog_data,
        title.to_il2cpp_string(),
        message.to_il2cpp_string(),
        null_mut(),
        TextId::from_name("Common0007"),
        typ
    );
    PushDialog(dialog_data);
    // PushSystemDialog(dialog_data, true);
}

pub fn three_button_message(
    title: &str, message: &str, typ: FormType,
    on_right: *mut Il2CppDelegate, right_text: &str,
    on_center: *mut Il2CppDelegate, center_text: &str
) {
    let dialog_data = Data::new();
    let close_text_id = TextId::from_name("Common0007");
    Data::SetSimpleThreeButtonMessage(
        dialog_data,
        title.to_il2cpp_string(),
        message.to_il2cpp_string(),
        on_right,
        close_text_id,
        on_center,
        close_text_id,
        null_mut(),
        close_text_id,
        typ
    );
    Data::set_CenterButtonText(dialog_data, center_text.to_il2cpp_string());
    Data::set_RightButtonText(dialog_data, right_text.to_il2cpp_string());
    PushDialog(dialog_data);
}

pub fn init(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, DialogManager);

    unsafe {
        CLASS = DialogManager;
        PUSHDIALOG_ADDR = get_method_addr(DialogManager, c"PushDialog", 1);
        PUSHSYSTEMDIALOG_ADDR =get_method_addr(DialogManager, c"PushSystemDialog", 2);
        GETFOREFRONTDIALOG_METHOD = get_method(DialogManager, c"GetForeFrontDialog", 0).unwrap_or_else(|e| {
            warn!("{}", e);
            std::ptr::null()
        });
    }
}
