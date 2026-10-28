// A2U flash animations (pop-ups, banners, result effects) at ui_animation_scale, opt-in via
// ui_animation_scale_flash. DOTween tweens get the same scale in DOTween/TweenManager.rs.
use crate::{
    core::Hachimi,
    il2cpp::{
        api::{il2cpp_field_get_flags, il2cpp_field_get_type, il2cpp_type_get_type},
        symbols::{get_field_from_name, get_field_ptr, get_method_addr},
        types::*
    }
};

// _customTimeScale, the field behind the library's own SetCustomTimeScale
static mut CUSTOM_TIME_SCALE_FIELD: *mut FieldInfo = 0 as _;

// ponytail: Update is the only per-frame method in AnimateToUnity, so it drives every flash, and it is
// assumed to multiply its frame delta by _customTimeScale (names only, the game's code is packed).
// If flashes don't speed up in game, scale AnRoot::_deltaTime after AnRoot::_UpdateTime instead.
type UpdateFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn AnRootManager_Update(this: *mut Il2CppObject) {
    let (on, scale) = {
        let config = Hachimi::instance().config.load();
        (config.ui_animation_scale_flash, config.ui_animation_scale)
    };
    if !on || !(scale > 0.0) || scale == 1.0 { // also rejects NaN and a hand-edited 0 or below
        return get_orig_fn!(AnRootManager_Update, UpdateFn)(this);
    }

    let p = get_field_ptr::<f32>(this, unsafe { CUSTOM_TIME_SCALE_FIELD });
    let game = unsafe { *p };
    let ours = game * scale;
    unsafe { *p = ours };
    get_orig_fn!(AnRootManager_Update, UpdateFn)(this);
    // the game's value back, unless it set its own during the tick
    if unsafe { *p } == ours {
        unsafe { *p = game };
    }
}

pub fn init(Plugins: *const Il2CppImage) {
    get_class_or_return!(Plugins, AnimateToUnity, AnRootManager);

    // only an instance float is written through; any other layout leaves flashes alone
    let field = get_field_from_name(AnRootManager, c"_customTimeScale");
    if field.is_null()
        || (il2cpp_field_get_flags(field) & 0x10) != 0 // FIELD_ATTRIBUTE_STATIC
        || il2cpp_type_get_type(il2cpp_field_get_type(field)) != Il2CppTypeEnum_IL2CPP_TYPE_R4 as i32
    {
        warn!("AnRootManager: _customTimeScale is not an instance float, flash speed-up disabled");
        return;
    }
    unsafe { CUSTOM_TIME_SCALE_FIELD = field };

    let Update_addr = get_method_addr(AnRootManager, c"Update", 0);
    new_hook!(Update_addr, AnRootManager_Update);
}
