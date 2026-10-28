// Career timer: one line per finished career in <data dir>/career_times.log. The running total
// is kept in career_timer.json, so a career played over several game sessions adds up.
// Counted time = time on career screens (the career scene, plus story scenes opened from it).
use std::{ffi::CStr, fs::OpenOptions, io::Write, path::PathBuf, sync::Mutex, time::{Duration, Instant}};

use serde::{Deserialize, Serialize};

use crate::{
    core::{utils, Hachimi},
    il2cpp::{ext::Il2CppObjectExt, symbols::{invoke_object_method, unbox}}
};
use super::{SceneDefine::SceneId, SceneManager, WorkDataManager};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Saved {
    started: String, // local time the timer first saw this career
    first_turn: i32,
    last_turn: i32,
    secs: f64,
    sessions: u32
}

struct State {
    saved: Option<Saved>,
    loaded: bool,         // the file was read this session
    resume_pending: bool, // count this session once the first turn is read
    finished: bool,    // result screen seen; wait until the career scene is left
    last_real_scene: i32,
    last_tick: Option<Instant>,
    next_poll: Instant,
    next_save: Instant
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

const POLL: Duration = Duration::from_secs(1);
const SAVE_EVERY: Duration = Duration::from_secs(30);
const MAX_FRAME: Duration = Duration::from_secs(5); // a longer gap (hang, debugger) is not counted

fn dir() -> PathBuf {
    Hachimi::instance().game.data_dir.clone()
}

fn current_turn() -> Option<i32> {
    let wdm = WorkDataManager::instance();
    if wdm.is_null() {
        return None;
    }
    let single_mode = invoke_object_method(wdm, c"get_SingleMode", 0, &mut []).filter(|p| !p.is_null())?;
    let turn = invoke_object_method(single_mode, c"GetCurrentTurn", 0, &mut []).filter(|p| !p.is_null())?;
    let turn = unsafe { unbox::<i32>(turn) };
    (1..=200).contains(&turn).then_some(turn)
}

fn on_result_screen() -> bool {
    let vc = SceneManager::current_view_controller();
    !vc.is_null() && unsafe { CStr::from_ptr((*(*vc).klass()).name) } == c"SingleModeResultViewController"
}

fn now_text() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

fn fmt_secs(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 { format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60) } else { format!("{}m {:02}s", s / 60, s % 60) }
}

// A turn lower than the saved one means a new career; otherwise the saved one continues.
fn track(saved: Option<Saved>, turn: i32, resumed: bool) -> Saved {
    match saved {
        Some(mut s) if turn >= s.last_turn => {
            s.last_turn = turn;
            if resumed {
                s.sessions += 1;
            }
            s
        }
        _ => Saved { started: now_text(), first_turn: turn, last_turn: turn, secs: 0.0, sessions: 1 }
    }
}

fn line(s: &Saved) -> String {
    format!("{}  career finished  turns {}-{}  time {}  sessions {}  (started {})",
        now_text(), s.first_turn, s.last_turn, fmt_secs(s.secs), s.sessions, s.started)
}

fn save(saved: &Option<Saved>) {
    let path = dir().join("career_timer.json");
    let res = match saved {
        Some(s) => utils::write_json_file(s, &path).map_err(|e| e.to_string()),
        None => std::fs::remove_file(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }).map_err(|e| e.to_string())
    };
    if let Err(e) = res {
        warn!("career timer: save failed: {}", e);
    }
}

fn load() -> Option<Saved> {
    let text = std::fs::read_to_string(dir().join("career_timer.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn finish(s: &Saved) {
    let text = line(s);
    info!("{}", text);
    let res = OpenOptions::new().create(true).append(true).open(dir().join("career_times.log"))
        .and_then(|mut f| writeln!(f, "{}", text));
    if let Err(e) = res {
        warn!("career timer: write failed: {}", e);
    }
}

// Main thread, every frame (GameSystem::Update).
pub fn tick() {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let st = guard.get_or_insert_with(|| State {
        saved: None, loaded: false, resume_pending: false, finished: false, last_real_scene: 0, last_tick: None, next_poll: now, next_save: now + SAVE_EVERY
    });

    let scene = SceneManager::current_scene_id() as i32;
    let career = scene == SceneId::SingleMode as i32
        || ((scene == SceneId::Story as i32 || scene == SceneId::None as i32) && st.last_real_scene == SceneId::SingleMode as i32);
    if scene != SceneId::Story as i32 && scene != SceneId::None as i32 {
        st.last_real_scene = scene;
    }

    if !career {
        if st.last_tick.take().is_some() && st.saved.is_some() {
            save(&st.saved); // left the career screens: keep the total safe
        }
        st.finished = false;
        return;
    }

    if let Some(prev) = st.last_tick.replace(now) {
        let dt = now - prev;
        if let Some(s) = st.saved.as_mut().filter(|_| dt <= MAX_FRAME && !st.finished) {
            s.secs += dt.as_secs_f64();
        }
    }

    if now < st.next_poll || st.finished {
        return;
    }
    st.next_poll = now + POLL;

    if !st.loaded {
        st.loaded = true;
        st.resume_pending = true;
        st.saved = load();
    }

    if on_result_screen() {
        if let Some(mut s) = st.saved.take() {
            s.last_turn = current_turn().map_or(s.last_turn, |t| t.max(s.last_turn));
            finish(&s);
        }
        save(&None);
        st.finished = true;
        return;
    }

    let Some(turn) = current_turn() else { return };
    st.saved = Some(track(st.saved.take(), turn, std::mem::take(&mut st.resume_pending)));

    if now >= st.next_save {
        st.next_save = now + SAVE_EVERY;
        save(&st.saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn career_timer_tracking() {
        let first = track(None, 1, true);
        assert_eq!((first.first_turn, first.last_turn, first.sessions), (1, 1, 1));
        let mid = track(Some(Saved { secs: 60.0, ..first.clone() }), 30, false);
        assert_eq!((mid.last_turn, mid.sessions, mid.secs), (30, 1, 60.0));
        let resumed = track(Some(mid.clone()), 30, true);
        assert_eq!(resumed.sessions, 2, "same turn after a restart continues");
        let fresh = track(Some(mid), 1, true);
        assert_eq!((fresh.first_turn, fresh.secs, fresh.sessions), (1, 0.0, 1), "lower turn: new career");
        assert_eq!(fmt_secs(1872.4), "31m 12s");
        assert_eq!(fmt_secs(3723.0), "1h 02m 03s");
    }
}
