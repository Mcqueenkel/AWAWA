use std::{sync::Mutex, time::{Duration, Instant}};

use crate::{
    core::Hachimi,
    windows::free_camera::{self, CameraScene},
    il2cpp::{
        ext::Il2CppStringExt,
        hook::UnityEngine_CoreModule::{Camera, Object, Transform},
        symbols::{get_method_addr, get_method_overload_addr, Array},
        types::*,
    },
};

use super::{HorseRaceInfo, RaceHorseManagerBase, RaceHorseManagerReplay, RaceManager, RaceManagerReplayBase};

def_method_wrapper_fn!(get_CurrentCamera, GET_CURRENT_CAMERA_ADDR, *mut Il2CppObject, this: *mut Il2CppObject);

// Name tag over one Uma; x, y are 0..1 from the top-left of the game view.
#[derive(Clone)]
pub struct NameMarker {
    pub x: f32,
    pub y: f32,
    pub name: String,
}

// Written on the main thread after the race camera is final, read by the GUI.
static NAME_MARKERS: Mutex<(Option<Instant>, Vec<NameMarker>)> = Mutex::new((None, Vec::new()));
// AlterLateUpdate stops when the race scene dies, so an old list must expire.
const NAME_MARKER_TTL: Duration = Duration::from_millis(250);
// ponytail: fixed lift over the sim position; switch to RaceModelController::get_HeadTransform if tags float off the heads
const NAME_MARKER_LIFT: f32 = 2.0;

fn is_fresh(stamp: Option<Instant>) -> bool {
    stamp.is_some_and(|t| t.elapsed() < NAME_MARKER_TTL)
}

pub fn name_markers() -> Option<Vec<NameMarker>> {
    let markers = NAME_MARKERS.lock().unwrap();
    is_fresh(markers.0).then(|| markers.1.clone())
}

pub fn has_name_markers() -> bool {
    is_fresh(NAME_MARKERS.lock().unwrap().0)
}

// Unity viewport (origin bottom-left) to top-left 0..1; None when behind the camera or off screen.
fn viewport_to_top_left(v: Vector3_t) -> Option<(f32, f32)> {
    (v.z > 0.0 && (0.0..=1.0).contains(&v.x) && (0.0..=1.0).contains(&v.y)).then(|| (v.x, 1.0 - v.y))
}

// Uma names only (never trainer names). Hidden outside replays and while a cut-in plays.
fn collect_name_markers(this: *mut Il2CppObject) -> Option<Vec<NameMarker>> {
    if !Hachimi::instance().config.load().windows.race_name_markers || unsafe { GET_CURRENT_CAMERA_ADDR } == 0 {
        return None;
    }
    let race_manager = RaceManager::instance();
    if race_manager.is_null() {
        return None;
    }
    // a replay horse manager means race_manager is a RaceManagerReplayBase
    let horse_manager = RaceManager::get__horseManager(race_manager);
    if !RaceHorseManagerReplay::is_replay_manager(horse_manager) ||
        RaceManagerReplayBase::IsCutInPlayingOrReserved(race_manager)
    {
        return None;
    }
    let camera = get_CurrentCamera(this);
    if camera.is_null() || !Object::IsNativeObjectAlive(camera) {
        return None;
    }
    let infos = RaceHorseManagerBase::GetHorseRaceInfos(horse_manager);
    if infos.is_null() {
        return None;
    }
    let infos: Array<*mut Il2CppObject> = Array::from(infos);

    let mut markers = Vec::new();
    // ponytail: names are converted every frame (at most 18 short strings); cache by pointer if it shows in a profile
    for &info in unsafe { infos.as_slice() }.iter() {
        if info.is_null() {
            continue;
        }
        let name = HorseRaceInfo::get_CharaName(info);
        if name.is_null() {
            continue;
        }
        let mut pos = HorseRaceInfo::get__position(info);
        pos.y += NAME_MARKER_LIFT;
        let Some((x, y)) = Camera::world_to_viewport(camera, pos).and_then(viewport_to_top_left) else {
            continue;
        };
        markers.push(NameMarker { x, y, name: unsafe { (*name).as_utf16str().to_string() } });
    }
    Some(markers)
}

type NoArgsFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn RaceCameraManager_AlterLateUpdate(this: *mut Il2CppObject) {
    free_camera::set_race_active();
    free_camera::tick();

    let active = free_camera::is_scene_enabled(CameraScene::Race);
    Transform::set_update_race_camera(active);
    get_orig_fn!(RaceCameraManager_AlterLateUpdate, NoArgsFn)(this);
    Transform::set_update_race_camera(false);

    // never hold NAME_MARKERS across il2cpp calls
    let markers = collect_name_markers(this).unwrap_or_default();
    *NAME_MARKERS.lock().unwrap() = (if markers.is_empty() { None } else { Some(Instant::now()) }, markers);
}

type RaceChangeCameraModeFn = extern "C" fn(this: *mut Il2CppObject, mode: i32, is_skip: bool);
extern "C" fn RaceCameraManager_ChangeCameraMode(this: *mut Il2CppObject, mode: i32, is_skip: bool) {
    if free_camera::is_scene_enabled(CameraScene::Race) {
        return;
    }
    get_orig_fn!(RaceCameraManager_ChangeCameraMode, RaceChangeCameraModeFn)(this, mode, is_skip);
}

// public bool PlayEventCamera(int targetHorseIndex, int[] rivalHorseIndexArray, int cameraId, bool isForceInPlaying = False, bool isForceUnPlayableArea = False) { }
type RacePlayEventCameraFn = extern "C" fn(
    this: *mut Il2CppObject,
    targetHorseIndex: i32,
    rivalHorseIndexArray: *mut Il2CppArray,
    cameraId: i32,
    isForceInPlaying: bool,
    isForceUnPlayableArea: bool,
) -> bool;
extern "C" fn RaceCameraManager_PlayEventCamera(
    this: *mut Il2CppObject,
    targetHorseIndex: i32,
    rivalHorseIndexArray: *mut Il2CppArray,
    cameraId: i32,
    isForceInPlaying: bool,
    isForceUnPlayableArea: bool,
) -> bool {
    if free_camera::is_scene_enabled(CameraScene::Race) {
        return false;
    }
    get_orig_fn!(RaceCameraManager_PlayEventCamera, RacePlayEventCameraFn)(this, targetHorseIndex, rivalHorseIndexArray, cameraId, isForceInPlaying, isForceUnPlayableArea)
}

pub fn init(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, "Gallop", RaceCameraManager);

    unsafe {
        GET_CURRENT_CAMERA_ADDR = get_method_addr(RaceCameraManager, c"get_CurrentCamera", 0);
    }

    let RaceCameraManager_AlterLateUpdate_addr = get_method_addr(RaceCameraManager, c"AlterLateUpdate", 0);
    new_hook!(RaceCameraManager_AlterLateUpdate_addr, RaceCameraManager_AlterLateUpdate);

    let RaceCameraManager_ChangeCameraMode_addr = get_method_addr(RaceCameraManager, c"ChangeCameraMode", 2);
    new_hook!(RaceCameraManager_ChangeCameraMode_addr, RaceCameraManager_ChangeCameraMode);

    let RaceCameraManager_PlayEventCamera_addr = get_method_overload_addr(
        RaceCameraManager,
        "PlayEventCamera",
        &[
            Il2CppTypeEnum_IL2CPP_TYPE_I4, // int targetHorseIndex
            Il2CppTypeEnum_IL2CPP_TYPE_SZARRAY, // int[] rivalHorseIndexArray
            Il2CppTypeEnum_IL2CPP_TYPE_I4, // int cameraId
            Il2CppTypeEnum_IL2CPP_TYPE_BOOLEAN, // bool isForceInPlaying
            Il2CppTypeEnum_IL2CPP_TYPE_BOOLEAN // bool isForceUnPlayableArea
        ]
    );
    new_hook!(RaceCameraManager_PlayEventCamera_addr, RaceCameraManager_PlayEventCamera);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f32, y: f32, z: f32) -> Vector3_t {
        Vector3_t { x, y, z }
    }

    #[test]
    fn viewport_to_top_left_flips_and_culls() {
        assert_eq!(viewport_to_top_left(v(0.5, 0.5, 10.0)), Some((0.5, 0.5)));
        assert_eq!(viewport_to_top_left(v(0.25, 0.75, 5.0)), Some((0.25, 0.25)));
        assert_eq!(viewport_to_top_left(v(0.5, 0.5, -1.0)), None);
        assert_eq!(viewport_to_top_left(v(0.5, 0.5, 0.0)), None);
        assert_eq!(viewport_to_top_left(v(1.2, 0.5, 5.0)), None);
        assert_eq!(viewport_to_top_left(v(0.5, -0.1, 5.0)), None);
    }
}
