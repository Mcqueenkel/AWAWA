// Race data export: one JSON file per simulated race in <data dir>/race_data/. It holds a few ints from
// RaceInfo's own getters plus the game's base64 simulation blob, untouched. The blob only has frames,
// results and events keyed by horse index (no names, no ids); HorseData is never read.
use std::{ffi::CStr, path::PathBuf, sync::{Mutex, OnceLock}};

use chrono::{Local, NaiveDateTime};
use serde::Serialize;

use crate::{
    core::{utils, Hachimi},
    il2cpp::{
        api::{il2cpp_field_get_flags, il2cpp_field_get_name},
        ext::Il2CppStringExt,
        symbols::{find_nested_class, get_class, get_method_addr, get_static_field_value, invoke_object_method, Array, FieldsIter, GCHandle},
        types::*
    }
};
use super::SuperSkip::{invoke_get, lock};

// Enums are the game's raw values. Anything unreadable is null.
#[derive(Serialize)]
struct Record {
    exported_at: String,
    race_type: Option<i32>,
    race_instance_id: Option<i32>,
    track_id: Option<i32>,
    distance: Option<i32>,
    grade: Option<i32>,
    ground_condition: Option<i32>,
    weather: Option<i32>,
    player_horse_index: Option<i32>, // null when spectating
    finish_order: Option<Vec<i32>>,  // horse indexes, winner first
    sim_data_base64: String
}

static PENDING: Mutex<Vec<(GCHandle, GCHandle)>> = Mutex::new(Vec::new()); // strong: (RaceInfo, blob)
// blob hashes written this session: a re-import or watching after View Results writes nothing new.
// ponytail: linear scan, a few hundred races per session at most
static SEEN: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static RACE_TYPES: OnceLock<Vec<(i32, String)>> = OnceLock::new(); // RaceDefine.RaceType value -> name

// race_data/<time>_<race type name, or the number>_<hash>.json. The hash keeps races from the same
// second apart (Team Trials imports several at once).
fn rel_path(names: &[(i32, String)], race_type: i32, at: &NaiveDateTime, hash: u64) -> PathBuf {
    let name = names.iter().find(|(v, _)| *v == race_type).map_or_else(|| race_type.to_string(), |(_, n)| n.clone());
    PathBuf::from("race_data").join(format!("{}_{}_{:08x}.json", at.format("%Y%m%d-%H%M%S"), name, hash as u32))
}

fn record(race: *mut Il2CppObject, blob: *mut Il2CppString, exported_at: String) -> Record {
    let get = |name: &CStr| invoke_get::<i32>(race, name);
    Record {
        exported_at,
        race_type: get(c"get_RaceType"),
        race_instance_id: get(c"get_RaceInstanceId"),
        track_id: get(c"get_RaceTrackId"),
        distance: get(c"get_CourseDistance"),
        grade: get(c"get_Grade"),
        ground_condition: get(c"get_GroundCondition"),
        weather: get(c"get_Weather"),
        player_horse_index: get(c"get_PlayerHorseIndex").filter(|&i| i >= 0),
        // null until the game has cached the order
        finish_order: invoke_object_method(race, c"get_HorseIndexByFinishOrder", 0, &mut [])
            .filter(|p| !p.is_null())
            .map(|p| unsafe { Array::<i32>::from(p as *mut Il2CppArray).as_slice() }.to_vec()),
        sim_data_base64: unsafe { (*blob).as_utf16str() }.to_string()
    }
}

// Main thread, every frame (GameSystem::Update). Reads one frame after the hook, so the RaceInfo is set up.
pub fn tick() {
    let items = std::mem::take(&mut *lock(&PENDING));
    if items.is_empty() {
        return;
    }
    let names = RACE_TYPES.get().map_or(&[][..], |v| v.as_slice());
    let now = Local::now();
    let mut seen = lock(&SEEN);
    let jobs: Vec<_> = items.iter().filter_map(|(race, blob)| {
        let (race, blob) = (race.target(), blob.target() as *mut Il2CppString);
        if race.is_null() || blob.is_null() {
            return None;
        }
        let hash = unsafe { (*blob).hash() };
        if seen.contains(&hash) {
            return None;
        }
        seen.push(hash);
        let rec = record(race, blob, now.to_rfc3339());
        Some((rel_path(names, rec.race_type.unwrap_or(-1), &now.naive_local(), hash), rec))
    }).collect();
    drop(seen);
    drop(items); // frees the handles here, on the main thread
    if jobs.is_empty() {
        return;
    }

    let dir = Hachimi::instance().game.data_dir.clone();
    std::thread::spawn(move || {
        if let Err(e) = std::fs::create_dir_all(dir.join("race_data")) {
            warn!("race data: can't create folder: {}", e);
            return;
        }
        for (rel, rec) in jobs {
            let path = dir.join(rel);
            match utils::write_json_file(&rec, &path) {
                Ok(()) => info!("race data: wrote {}", path.display()),
                Err(e) => warn!("race data: write failed: {}", e)
            }
        }
    });
}

type SetAndDeserializeBase64Fn = extern "C" fn(this: *mut Il2CppObject, sim_data_base64: *mut Il2CppString);
extern "C" fn RaceInfo_SetAndDeserializeBase64(this: *mut Il2CppObject, sim_data_base64: *mut Il2CppString) {
    get_orig_fn!(RaceInfo_SetAndDeserializeBase64, SetAndDeserializeBase64Fn)(this, sim_data_base64);
    // only queue here: the game may still be filling the RaceInfo in
    if !this.is_null() && !sim_data_base64.is_null() && unsafe { (*sim_data_base64).length } > 0
        && Hachimi::instance().config.load().save_race_data {
        lock(&PENDING).push((GCHandle::new(this, false), GCHandle::new(sim_data_base64 as _, false)));
    }
}

pub fn init(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, RaceInfo);
    let SetAndDeserializeBase64_addr = get_method_addr(RaceInfo, c"SetAndDeserializeBase64", 1);
    new_hook!(SetAndDeserializeBase64_addr, RaceInfo_SetAndDeserializeBase64);

    // RaceType names for the file names; without them the number is used
    let names: Vec<(i32, String)> = get_class(umamusume, c"Gallop", c"RaceDefine").ok()
        .and_then(|c| find_nested_class(c, c"RaceType").ok())
        .map(|c| FieldsIter::new(c)
            .filter(|&f| il2cpp_field_get_flags(f) & 0x10 != 0) // static only: skips value__
            .map(|f| (get_static_field_value::<i32>(f), unsafe { CStr::from_ptr(il2cpp_field_get_name(f)) }.to_string_lossy().into_owned()))
            .collect())
        .unwrap_or_default();
    info!("race data: export ready ({} race type names)", names.len());
    _ = RACE_TYPES.set(names);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn race_export_paths() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 10, 29).unwrap().and_hms_opt(15, 30, 12).unwrap();
        let names = [(6, "Single".to_owned()), (15, "TeamStadium".to_owned())];
        assert_eq!(rel_path(&names, 6, &at, 0x1234_5678_9abc_def0), PathBuf::from("race_data/20261029-153012_Single_9abcdef0.json"));
        assert_eq!(rel_path(&names, 99, &at, 1), PathBuf::from("race_data/20261029-153012_99_00000001.json"), "unknown type: the number");
        assert_ne!(rel_path(&names, 15, &at, 2), rel_path(&names, 15, &at, 3), "same second, different races");
    }
}
