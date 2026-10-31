use crate::il2cpp::{api::il2cpp_resolve_icall, symbols::get_method_addr, types::*};

#[cfg(target_os = "windows")]
use crate::windows::free_camera::{self, CameraScene};
#[cfg(target_os = "windows")]
type CameraGetFloatFn = extern "C" fn(this: *mut Il2CppObject) -> f32;
#[cfg(target_os = "windows")]
type CameraSetFloatFn = extern "C" fn(this: *mut Il2CppObject, value: f32);

static mut SET_FIELD_OF_VIEW_ADDR: usize = 0;
impl_addr_wrapper_fn!(set_fieldOfView, SET_FIELD_OF_VIEW_ADDR, (), this: *mut Il2CppObject, value: f32);

static mut WORLD_TO_VIEWPORT_POINT_ADDR: usize = 0;
impl_addr_wrapper_fn!(WorldToViewportPoint, WORLD_TO_VIEWPORT_POINT_ADDR, Vector3_t, this: *mut Il2CppObject, position: Vector3_t);

// Viewport point (0..1, origin bottom-left, z = depth), or None when the method is missing.
pub fn world_to_viewport(cam: *mut Il2CppObject, position: Vector3_t) -> Option<Vector3_t> {
    (unsafe { WORLD_TO_VIEWPORT_POINT_ADDR } != 0).then(|| WorldToViewportPoint(cam, position))
}

#[cfg(target_os = "windows")]
fn should_override_near_clip() -> bool {
    free_camera::is_scene_enabled(CameraScene::Live) ||
        free_camera::is_scene_enabled(CameraScene::Race)
}

#[cfg(target_os = "windows")]
extern "C" fn Camera_get_fieldOfView(this: *mut Il2CppObject) -> f32 {
    let scene = free_camera::scene();
    if let Some(fov) = free_camera::fov_for_scene(scene) {
        return fov;
    }
    get_orig_fn!(Camera_get_fieldOfView, CameraGetFloatFn)(this)
}

#[cfg(target_os = "windows")]
extern "C" fn Camera_set_nearClipPlane(this: *mut Il2CppObject, mut value: f32) {
    if should_override_near_clip() {
        value = 0.001;
    }
    get_orig_fn!(Camera_set_nearClipPlane, CameraSetFloatFn)(this, value);
}

#[cfg(target_os = "windows")]
extern "C" fn Camera_get_nearClipPlane(this: *mut Il2CppObject) -> f32 {
    if should_override_near_clip() {
        return 0.001;
    }
    get_orig_fn!(Camera_get_nearClipPlane, CameraGetFloatFn)(this)
}

#[cfg(target_os = "windows")]
extern "C" fn Camera_set_farClipPlane(this: *mut Il2CppObject, mut value: f32) {
    if free_camera::is_scene_enabled(CameraScene::Live) || free_camera::is_scene_enabled(CameraScene::Race) {
        value = 2500.0;
    }
    get_orig_fn!(Camera_set_farClipPlane, CameraSetFloatFn)(this, value);
}

#[cfg(target_os = "windows")]
extern "C" fn Camera_get_farClipPlane(this: *mut Il2CppObject) -> f32 {
    if free_camera::is_scene_enabled(CameraScene::Live) || free_camera::is_scene_enabled(CameraScene::Race) {
        return 2500.0;
    }
    get_orig_fn!(Camera_get_farClipPlane, CameraGetFloatFn)(this)
}

pub fn init(UnityEngine_CoreModule: *const Il2CppImage) {
    unsafe {
        SET_FIELD_OF_VIEW_ADDR = il2cpp_resolve_icall(c"UnityEngine.Camera::set_fieldOfView(System.Single)".as_ptr());
    }

    #[cfg(target_os = "windows")]
    {
        let get_fieldOfView_addr = il2cpp_resolve_icall(c"UnityEngine.Camera::get_fieldOfView()".as_ptr());
        new_hook!(get_fieldOfView_addr, Camera_get_fieldOfView);

        let set_nearClipPlane_addr = il2cpp_resolve_icall(c"UnityEngine.Camera::set_nearClipPlane(System.Single)".as_ptr());
        new_hook!(set_nearClipPlane_addr, Camera_set_nearClipPlane);

        let get_nearClipPlane_addr = il2cpp_resolve_icall(c"UnityEngine.Camera::get_nearClipPlane()".as_ptr());
        new_hook!(get_nearClipPlane_addr, Camera_get_nearClipPlane);

        let set_farClipPlane_addr = il2cpp_resolve_icall(c"UnityEngine.Camera::set_farClipPlane(System.Single)".as_ptr());
        new_hook!(set_farClipPlane_addr, Camera_set_farClipPlane);

        let get_farClipPlane_addr = il2cpp_resolve_icall(c"UnityEngine.Camera::get_farClipPlane()".as_ptr());
        new_hook!(get_farClipPlane_addr, Camera_get_farClipPlane);
    }

    // last: get_class_or_return! returns early
    get_class_or_return!(UnityEngine_CoreModule, UnityEngine, Camera);
    unsafe {
        WORLD_TO_VIEWPORT_POINT_ADDR = get_method_addr(Camera, c"WorldToViewportPoint", 1);
    }
}
