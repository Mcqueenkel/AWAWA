// Auto-skip: calls the game's own skip methods (the ones its skip button uses) when a
// training cut-in or a story timeline starts, and cuts a few career animations short.
// Every action is behind a config toggle read on each call, so changes apply live.
use std::{
    ffi::CStr,
    ptr::null_mut,
    sync::{atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering}, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant}
};

use crate::{
    core::Hachimi,
    il2cpp::{
        api::{il2cpp_class_is_subclass_of, il2cpp_object_new, il2cpp_runtime_invoke},
        ext::{Il2CppObjectExt, Il2CppStringExt},
        hook::UnityEngine_CoreModule::{GameObject, Object},
        symbols::{get_field_from_name, get_field_object_value, get_field_value, get_method, get_method_addr, get_method_cached, get_static_field_value, invoke_object_method, unbox, Array, GCHandle, IEnumerator, MoveNextFn},
        types::*
    }
};
use super::{DialogManager, HorseRaceInfo, RaceHorseManagerBase, RaceManager, SceneDefine::SceneId, SceneManager};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn in_career() -> bool {
    SceneManager::current_scene_id() == SceneId::SingleMode
}

// ---- Skip callbacks ----
// When a skip replaces a call that would have fired a completion callback later, the callback
// fires on the next tick, after that call has returned to the game, as in the game's own flow.

static DEFERRED: Mutex<Vec<GCHandle>> = Mutex::new(Vec::new()); // strong handles
static DEFERRED_PENDING: AtomicBool = AtomicBool::new(false);

// Something we can Invoke() with no arguments (a System.Action). Checked before skipping, so a
// wrong argument (another layout on another client) runs the original instead of stalling.
fn invokable(cb: *mut Il2CppObject) -> bool {
    !cb.is_null() && get_method_cached(unsafe { (*cb).klass() }, c"Invoke", 0).is_ok()
}

fn defer_invoke(cb: *mut Il2CppObject) {
    if cb.is_null() {
        return;
    }
    lock(&DEFERRED).push(GCHandle::new(cb, false));
    DEFERRED_PENDING.store(true, Ordering::Release);
}

fn tick_deferred() {
    if !DEFERRED_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    let handles = std::mem::take(&mut *lock(&DEFERRED)); // unlocked: a callback may defer another
    for handle in handles {
        let cb = handle.target();
        if !cb.is_null() && invoke_object_method(cb, c"Invoke", 0, &mut []).is_none() {
            warn!("superskip: skip callback threw");
        }
    }
}

// ---- Training cut-ins ----

static TRAINING_SKIPPING: AtomicBool = AtomicBool::new(false);

fn skip_training(this: *mut Il2CppObject, from: &str) {
    if this.is_null() || !Hachimi::instance().config.load().skip_training_cutin || !in_career() {
        return;
    }
    // SkipRuntime may run the cut-in callbacks synchronously
    if TRAINING_SKIPPING.swap(true, Ordering::Acquire) {
        return;
    }
    let res = invoke_object_method(this, c"SkipRuntime", 0, &mut []);
    TRAINING_SKIPPING.store(false, Ordering::Release);

    if res.is_some() {
        info!("superskip: training SkipRuntime ({})", from);
    } else {
        warn!("superskip: training SkipRuntime failed ({})", from);
    }
}

type CutInFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn TrainingCutIn_OnPlayCutIn(this: *mut Il2CppObject) {
    get_orig_fn!(TrainingCutIn_OnPlayCutIn, CutInFn)(this);
    skip_training(this, "OnPlayCutIn");
}

extern "C" fn TrainingCutIn_OnPlayMainCutIn(this: *mut Il2CppObject) {
    get_orig_fn!(TrainingCutIn_OnPlayMainCutIn, CutInFn)(this);
    skip_training(this, "OnPlayMainCutIn");
}

// ---- Story timelines ----
// The hook only records the view; tick() skips once its timeline is playing.

const STORY_DEBOUNCE: Duration = Duration::from_millis(100);
const STORY_EXPIRY: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq)]
enum Gate { Skip, Wait, Drop }

// age: since the timeline started. since_skip: from the last accepted skip to that start,
// so a start caused by our own skip is ignored. Choices are never picked here.
fn story_gate(age: Duration, since_skip: Option<Duration>, training_cutt: bool, playing: bool) -> Gate {
    if age > STORY_EXPIRY || training_cutt || since_skip.is_some_and(|d| d < STORY_DEBOUNCE) {
        Gate::Drop
    } else if !playing {
        Gate::Wait
    } else {
        Gate::Skip
    }
}

struct Story {
    pending: Option<(GCHandle, Instant)>, // weak handle to the StoryViewController, start time
    last_skip: Option<Instant>
}
// ponytail: one slot, a newer start replaces an older one (only one story view plays at a time)
static STORY: Mutex<Story> = Mutex::new(Story { pending: None, last_skip: None });
static STORY_PENDING: AtomicBool = AtomicBool::new(false); // cheap per-frame check

fn story() -> MutexGuard<'static, Story> {
    lock(&STORY)
}

// ponytail: the Grand Live finale (last turn) is a story on the live top screen that waits for its
// Live button; SkipStory strands it there, so no story is skipped on that screen. Name match: a
// client without the class keeps today's behaviour. Upgrade path if a GrandLive story shows up on
// another screen: also drop when the timeline's CurrentPlayMode is PlayMode.GrandLive.
fn on_live_top() -> bool {
    let vc = SceneManager::current_view_controller();
    !vc.is_null() && unsafe { CStr::from_ptr((*(*vc).klass()).name) } == c"SingleModeScenarioLiveTopController"
}

fn story_enabled() -> bool {
    let config = Hachimi::instance().config.load();
    config.skip_story_events && (config.skip_story_everywhere || in_career()) && !on_live_top()
}

type OnStartPlayingTimelineFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn StoryView_OnStartPlayingTimeline(this: *mut Il2CppObject) {
    get_orig_fn!(StoryView_OnStartPlayingTimeline, OnStartPlayingTimelineFn)(this);
    if this.is_null() || !story_enabled() {
        return;
    }
    story().pending = Some((GCHandle::new_weak_ref(this, false), Instant::now()));
    STORY_PENDING.store(true, Ordering::Release);
}

static TICKS: AtomicU32 = AtomicU32::new(0);

// Main thread, every frame (GameSystem::Update).
pub fn tick() {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    tick_deferred();
    tick_shop(tick);
    tick_story();
    tick_race_result();
    tick_trace();
}

// ponytail: diagnostic, logs scene and screen changes so races on a new client can be mapped from
// the log; drop once the race-result arming is confirmed.
static TRACE_SCENE: AtomicI32 = AtomicI32::new(-1);
static TRACE_VIEW: AtomicUsize = AtomicUsize::new(0);
fn tick_trace() {
    let scene = SceneManager::current_scene_id() as i32;
    if TRACE_SCENE.swap(scene, Ordering::Relaxed) != scene {
        info!("superskip: scene {}", scene);
    }
    let vc = SceneManager::current_view_controller();
    let class = if vc.is_null() { null_mut() } else { unsafe { (*vc).klass() } };
    if TRACE_VIEW.swap(class as usize, Ordering::Relaxed) != class as usize && !class.is_null() {
        info!("superskip: view {}", unsafe { CStr::from_ptr((*class).name) }.to_string_lossy());
    }
}

fn tick_story() {
    if !STORY_PENDING.load(Ordering::Acquire) {
        return;
    }
    let (handle, started, last_skip) = {
        let mut st = story();
        STORY_PENDING.store(false, Ordering::Release);
        let Some((handle, started)) = st.pending.take() else { return };
        (handle, started, st.last_skip)
    };
    // the lock is released before any game call: SkipStory can start the next timeline synchronously

    // The view can be up to 2 s old (maybe destroyed), so every call goes through the runtime;
    // a failed call drops the skip.
    let view = handle.target();
    if view.is_null() || !story_enabled() {
        return;
    }
    let Some(timeline) = invoke_object_method(view, c"get_TimelineController", 0, &mut []) else { return };
    let playing = !timeline.is_null() && invoke_get::<bool>(timeline, c"get_IsPlaying") == Some(true);
    let training_cutt = invoke_get::<bool>(view, c"get_IsPlayingOrWillPlayTrainingCutt").unwrap_or(true);
    let since_skip = last_skip.map(|t| started.saturating_duration_since(t));

    match story_gate(started.elapsed(), since_skip, training_cutt, playing) {
        Gate::Drop => {}
        Gate::Wait => {
            let mut st = story();
            if st.pending.is_none() {
                st.pending = Some((handle, started));
                STORY_PENDING.store(true, Ordering::Release);
            }
        }
        Gate::Skip => {
            story().last_skip = Some(Instant::now()); // before the call, so a start it causes is debounced
            if invoke_object_method(view, c"SkipStory", 0, &mut []).is_some() {
                info!("superskip: story SkipStory");
            } else {
                warn!("superskip: story SkipStory failed");
            }
        }
    }
}

// ---- Shop item-use performance (scenario free shop) ----
// The coroutine is run to its end in one call (its waits are ignored), then the dialog that
// was in front before the drive is closed on a later tick, if it is still in front.

const SHOP_MAX_STEPS: u32 = 32;
const SHOP_CLOSE_WINDOW: Duration = Duration::from_secs(2);

static SHOP_DRIVING: AtomicBool = AtomicBool::new(false);
static SHOP_LEFT: AtomicUsize = AtomicUsize::new(0); // a capped enumerator, handed back to the game
static SHOP_CLOSE_PENDING: AtomicBool = AtomicBool::new(false);
// drive time, tick, weak handle to the dialog in front before the drive
static SHOP_CLOSE: Mutex<Option<(Instant, u32, GCHandle)>> = Mutex::new(None);

fn shop_enabled() -> bool {
    Hachimi::instance().config.load().skip_shop_performance && in_career()
}

#[derive(Debug, PartialEq)]
enum Drive { Done(u32), Capped, Threw(u32) }

// step: Some(moved) or None when the game threw.
fn drive(max: u32, mut step: impl FnMut() -> Option<bool>) -> Drive {
    for n in 1..=max {
        match step() {
            Some(true) => {}
            Some(false) => return Drive::Done(n),
            None => return Drive::Threw(n)
        }
    }
    Drive::Capped
}

// Never on the drive's own tick, so the game's completion code runs first.
fn shop_close_gate(age: Duration, armed_tick: u32, tick: u32) -> Gate {
    if age > SHOP_CLOSE_WINDOW {
        Gate::Drop
    } else if tick == armed_tick {
        Gate::Wait
    } else {
        Gate::Skip
    }
}

extern "C" fn ShopUsePerf_MoveNext(enumerator: *mut Il2CppObject) -> bool {
    let orig = get_orig_fn!(ShopUsePerf_MoveNext, MoveNextFn);
    if enumerator.is_null() || SHOP_DRIVING.load(Ordering::Acquire)
        || SHOP_LEFT.load(Ordering::Relaxed) == enumerator as usize || !shop_enabled() {
        return orig(enumerator);
    }
    let front = DialogManager::GetForeFrontDialog().unwrap_or(null_mut());

    // Each step goes through the runtime (so a game exception is caught) and back into this
    // hook, which passes it straight to the original while SHOP_DRIVING is set.
    SHOP_DRIVING.store(true, Ordering::Release);
    let res = drive(SHOP_MAX_STEPS, || invoke_object_method(enumerator, c"MoveNext", 0, &mut [])
        .map(|r| !r.is_null() && unsafe { unbox::<bool>(r) }));
    SHOP_DRIVING.store(false, Ordering::Release);

    match res {
        Drive::Done(n) => {
            info!("superskip: shop performance driven in {} steps (dialog in front {:p})", n, front);
            if !front.is_null() {
                *lock(&SHOP_CLOSE) = Some((Instant::now(), TICKS.load(Ordering::Relaxed), GCHandle::new_weak_ref(front, false)));
                SHOP_CLOSE_PENDING.store(true, Ordering::Release);
            }
            false
        }
        Drive::Capped => {
            SHOP_LEFT.store(enumerator as usize, Ordering::Relaxed);
            warn!("superskip: shop performance not done after {} steps, left to the game", SHOP_MAX_STEPS);
            true
        }
        Drive::Threw(n) => {
            warn!("superskip: shop performance threw at step {}", n);
            false // an exception ends a coroutine in the game too
        }
    }
}

type PlayUseItemPerformanceCoroutineFn = extern "C" fn(
    this: *mut Il2CppObject, hp_gauge: *mut Il2CppObject, motivation_button: *mut Il2CppObject,
    item_id_list: *mut Il2CppObject, on_complete_initialize_flash: *mut Il2CppObject,
    on_close_wipe: *mut Il2CppObject, on_complete_end: *mut Il2CppObject
) -> *mut Il2CppObject;
extern "C" fn ShopUsePerf_PlayUseItemPerformanceCoroutine(
    this: *mut Il2CppObject, hp_gauge: *mut Il2CppObject, motivation_button: *mut Il2CppObject,
    item_id_list: *mut Il2CppObject, on_complete_initialize_flash: *mut Il2CppObject,
    on_close_wipe: *mut Il2CppObject, on_complete_end: *mut Il2CppObject
) -> *mut Il2CppObject {
    let enumerator = get_orig_fn!(ShopUsePerf_PlayUseItemPerformanceCoroutine, PlayUseItemPerformanceCoroutineFn)(
        this, hp_gauge, motivation_button, item_id_list, on_complete_initialize_flash, on_close_wipe, on_complete_end
    );
    SHOP_LEFT.store(0, Ordering::Relaxed); // a new enumerator may reuse the old address
    if !enumerator.is_null() && shop_enabled() {
        if let Err(e) = IEnumerator::<*mut Il2CppObject>::from(enumerator).hook_move_next(ShopUsePerf_MoveNext) {
            error!("superskip: shop MoveNext hook failed: {}", e);
        }
    }
    enumerator
}

fn tick_shop(tick: u32) {
    if !SHOP_CLOSE_PENDING.load(Ordering::Acquire) {
        return;
    }
    let dialog = {
        let mut slot = lock(&SHOP_CLOSE);
        let gate = match &*slot {
            Some((at, armed, _)) => shop_close_gate(at.elapsed(), *armed, tick),
            None => Gate::Drop
        };
        if gate == Gate::Wait {
            return;
        }
        let taken = slot.take();
        SHOP_CLOSE_PENDING.store(false, Ordering::Release);
        match taken {
            Some((_, _, handle)) if gate == Gate::Skip && shop_enabled() => handle.target(),
            _ => return
        }
    };

    // At most one dialog: the one that was in front before the drive, still in front and not
    // already closing. A dialog pushed since (an alert, a retry) is left alone.
    let front = DialogManager::GetForeFrontDialog();
    if dialog.is_null() || front != Some(dialog) || invoke_get::<bool>(dialog, c"get_IsDispClosing") != Some(false) {
        info!("superskip: shop nothing closed (before {:p}, now {:?})", dialog, front);
        return;
    }
    if invoke_object_method(dialog, c"Close", 0, &mut []).is_some() {
        info!("superskip: shop closed dialog {:p}", dialog);
    } else {
        warn!("superskip: shop dialog Close failed");
    }
}

// ---- Rival entry animation (career race entry) ----

static mut PADDOCK_CLASS: *mut Il2CppClass = null_mut();

// False on any paddock view (paddocks embed the same animation), and when the view is unknown.
fn outside_paddock() -> bool {
    let vc = SceneManager::current_view_controller();
    !vc.is_null() && !il2cpp_class_is_subclass_of(unsafe { (*vc).klass() }, unsafe { PADDOCK_CLASS }, false)
}

fn rival_enabled() -> bool {
    Hachimi::instance().config.load().skip_rival_entry && in_career()
}

// <>1__state, <>4__this, endAction; None (logged once) when any is missing.
// ponytail: resolved from the first enumerator, since only this coroutine's class routes here
static RIVAL_FIELDS: OnceLock<Option<[usize; 3]>> = OnceLock::new();

fn rival_fields(klass: *mut Il2CppClass) -> Option<[*mut FieldInfo; 3]> {
    let fields = RIVAL_FIELDS.get_or_init(|| {
        let f = [c"<>1__state", c"<>4__this", c"endAction"].map(|n| get_field_from_name(klass, n) as usize);
        (!f.contains(&0)).then_some(f)
    });
    (*fields).map(|f| f.map(|a| a as *mut FieldInfo))
}

extern "C" fn RivalEntry_MoveNext(enumerator: *mut Il2CppObject) -> bool {
    let orig = get_orig_fn!(RivalEntry_MoveNext, MoveNextFn);
    if enumerator.is_null() || !rival_enabled() {
        return orig(enumerator);
    }
    let Some([state_field, this_field, end_field]) = rival_fields(unsafe { (*enumerator).klass() }) else {
        return orig(enumerator);
    };
    // first step only (later on the game may already have fired endAction), never in a paddock
    if get_field_value::<i32>(enumerator, state_field) != 0 || !outside_paddock() {
        return orig(enumerator);
    }
    let this = get_field_object_value::<Il2CppObject>(enumerator, this_field);
    let end_action = get_field_object_value::<Il2CppObject>(enumerator, end_field);
    // endAction must be callable before anything is destroyed
    if this.is_null() || !(end_action.is_null() || invokable(end_action))
        || invoke_object_method(this, c"DestroyRivalEntryWithUnload", 0, &mut []).is_none() {
        return orig(enumerator);
    }
    defer_invoke(end_action);
    info!("superskip: rival entry skipped");
    false
}

type PlayRivalEntryCoroutineFn = extern "C" fn(this: *mut Il2CppObject, end_action: *mut Il2CppObject, item_icon_list: *mut Il2CppObject) -> *mut Il2CppObject;
extern "C" fn RivalEntry_PlayRivalEntryCoroutine(this: *mut Il2CppObject, end_action: *mut Il2CppObject, item_icon_list: *mut Il2CppObject) -> *mut Il2CppObject {
    let enumerator = get_orig_fn!(RivalEntry_PlayRivalEntryCoroutine, PlayRivalEntryCoroutineFn)(this, end_action, item_icon_list);
    if !enumerator.is_null() && rival_enabled() {
        if let Err(e) = IEnumerator::<*mut Il2CppObject>::from(enumerator).hook_move_next(RivalEntry_MoveNext) {
            error!("superskip: rival MoveNext hook failed: {}", e);
        }
    }
    enumerator
}

// ---- Tag (friendship) training cut-in ----
// PlayCutIn(partnerList, Action onInEnd) plays the in part, PlayCutInOut(Action onEnd = null)
// the out part. A skip hides the cut-in root and fires the callback on the next tick instead.

static TAG_ROOT_HIDDEN: AtomicBool = AtomicBool::new(false); // we deactivated the cut-in root
static TAG_IN_SKIPPED: AtomicBool = AtomicBool::new(false); // the last in part was skipped
static mut TAG_ROOT_FIELD: *mut FieldInfo = null_mut(); // _tagCutInRootObject; null: hide nothing

#[derive(Debug, PartialEq)]
enum Tag { Original, RestoreThenOriginal, Skip }

// callback_ok: in part, an invokable callback; out part, none or an invokable one.
// An out part after a skipped in part is skipped too, even with the toggle off, since the in
// part never set it up. root_hidden: an earlier skip deactivated the cut-in root.
fn tag_action(enabled: bool, callback_ok: bool, out_part: bool, in_skipped: bool, root_hidden: bool) -> Tag {
    let skip = callback_ok && if out_part { enabled || in_skipped } else { enabled };
    if skip {
        Tag::Skip
    } else if root_hidden && !out_part {
        Tag::RestoreThenOriginal
    } else {
        Tag::Original
    }
}

fn tag_root(this: *mut Il2CppObject) -> *mut Il2CppObject {
    let field = unsafe { TAG_ROOT_FIELD };
    if field.is_null() { null_mut() } else { get_field_object_value(this, field) }
}

// true when the original must not run
fn tag_cutin(this: *mut Il2CppObject, callback: *mut Il2CppObject, out_part: bool) -> bool {
    if this.is_null() {
        return false;
    }
    let enabled = Hachimi::instance().config.load().skip_tag_cutin && in_career();
    let callback_ok = invokable(callback) || (out_part && callback.is_null());
    let action = tag_action(enabled, callback_ok, out_part, TAG_IN_SKIPPED.load(Ordering::Acquire), TAG_ROOT_HIDDEN.load(Ordering::Acquire));
    TAG_IN_SKIPPED.store(!out_part && action == Tag::Skip, Ordering::Release);
    match action {
        Tag::Original => false,
        Tag::RestoreThenOriginal => {
            let root = tag_root(this);
            if !root.is_null() {
                GameObject::SetActive(root, true);
            }
            TAG_ROOT_HIDDEN.store(false, Ordering::Release);
            false
        }
        Tag::Skip => {
            // only the cut-in root: the player's own object also runs the button and result effects
            let root = tag_root(this);
            if !root.is_null() && GameObject::get_activeSelf(root) {
                GameObject::SetActive(root, false);
                TAG_ROOT_HIDDEN.store(true, Ordering::Release);
            }
            defer_invoke(callback);
            info!("superskip: tag cut-in skipped ({})", if out_part { "PlayCutInOut" } else { "PlayCutIn" });
            true
        }
    }
}

type TagPlayCutInFn = extern "C" fn(this: *mut Il2CppObject, partner_list: *mut Il2CppObject, on_in_end: *mut Il2CppObject);
extern "C" fn TagCutIn_PlayCutIn(this: *mut Il2CppObject, partner_list: *mut Il2CppObject, on_in_end: *mut Il2CppObject) {
    if !tag_cutin(this, on_in_end, false) {
        get_orig_fn!(TagCutIn_PlayCutIn, TagPlayCutInFn)(this, partner_list, on_in_end);
    }
}

type TagPlayCutInOutFn = extern "C" fn(this: *mut Il2CppObject, on_end: *mut Il2CppObject);
extern "C" fn TagCutIn_PlayCutInOut(this: *mut Il2CppObject, on_end: *mut Il2CppObject) {
    if !tag_cutin(this, on_end, true) {
        get_orig_fn!(TagCutIn_PlayCutInOut, TagPlayCutInOutFn)(this, on_end);
    }
}

// ---- Race result (career races won) ----
// Armed when the scene goes SingleMode -> Race. Once the result screen is up and the final
// order says the player won a career race, the result buttons are clicked like a tap would.
// The race skip button is never pressed, and nothing is pressed after a loss.

const RR_PRESS_GAP: Duration = Duration::from_millis(300);
const RR_WINDOW: Duration = Duration::from_secs(90);
const RR_BUTTONS: [&str; 3] = ["NextButton", "ScreenTap", "SingleModeNextButton"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Rr {
    Idle,
    Armed, // career race running, result not decided
    Won { until: Instant, next_scan: Instant }
}

static RR: Mutex<Rr> = Mutex::new(Rr::Idle);
static RR_READY: AtomicBool = AtomicBool::new(false); // init resolved everything below
static RR_ACTIVE: AtomicBool = AtomicBool::new(false); // RR != Idle
static RR_SCAN: AtomicBool = AtomicBool::new(false); // won window: buttons may be pressed this frame
static RR_LAST_SCENE: AtomicI32 = AtomicI32::new(0);
// RaceDefine.RaceType.Single and HorseRaceInfo.Rank1stNo, read from the game at init
// (6 and 0 on Global), so another client's numbering can't make a team race look single
static RR_RACE_SINGLE: AtomicI32 = AtomicI32::new(-1);
static RR_RANK_1ST: AtomicI32 = AtomicI32::new(-1);
static mut RR_VIEW_CLASS: *mut Il2CppClass = null_mut();
static mut RR_RESULT_UI_FIELD: *mut FieldInfo = null_mut();
static mut RR_ES_CURRENT: *const MethodInfo = std::ptr::null();
static mut RR_POINTER_EVENT_DATA_CLASS: *mut Il2CppClass = null_mut();

fn rr_arms(from: i32, scene: i32) -> bool {
    from == SceneId::SingleMode as i32 && scene == SceneId::Race as i32
}

// (from, new last): loading frames (SceneId::None) keep the last real scene.
fn rr_from(last: i32, scene: i32) -> (i32, i32) {
    if scene == SceneId::None as i32 { (last, last) } else { (last, scene) }
}

// One tick. from: the last real scene before this tick. decide: Some(won) once the final
// result is known; only called while armed. Returns the next state and whether to scan.
fn rr_step(st: Rr, enabled: bool, from: i32, scene: i32, now: Instant, decide: impl FnOnce() -> Option<bool>) -> (Rr, bool) {
    if !enabled {
        return (Rr::Idle, false);
    }
    match st {
        Rr::Idle => (if rr_arms(from, scene) { Rr::Armed } else { Rr::Idle }, false),
        _ if scene == SceneId::None as i32 => (st, false), // loading frame: hold, never press
        _ if scene != SceneId::Race as i32 => (Rr::Idle, false),
        Rr::Armed => match decide() {
            None => (Rr::Armed, false),
            Some(true) => (Rr::Won { until: now + RR_WINDOW, next_scan: now }, false),
            Some(false) => (Rr::Idle, false)
        },
        Rr::Won { until, .. } if now >= until => (Rr::Idle, false),
        Rr::Won { until, next_scan } if now >= next_scan => (Rr::Won { until, next_scan: now + RR_PRESS_GAP }, true),
        won => (won, false)
    }
}

// Won only in a career race where the player is 1st (order 0) and every other horse placed
// behind. Fails safe: unset orders (all 0), a tie, or a 1-based order never pass.
// single, rank_1st: the game's RaceType.Single and Rank1stNo.
fn race_won(race_type: i32, single: i32, rank_1st: i32, player: usize, orders: impl IntoIterator<Item = Option<i32>>) -> bool {
    if race_type != single {
        return false;
    }
    let mut first = false;
    for (i, order) in orders.into_iter().enumerate() {
        match order {
            None => return false,
            Some(o) if i == player => first = o == rank_1st,
            Some(o) if o <= rank_1st => return false,
            _ => {}
        }
    }
    first
}

fn invoke_get<T: Copy>(obj: *mut Il2CppObject, name: &CStr) -> Option<T> {
    invoke_object_method(obj, name, 0, &mut []).filter(|r| !r.is_null()).map(|r| unsafe { unbox::<T>(r) })
}

fn rr_result_ui_up() -> bool {
    let vc = SceneManager::current_view_controller();
    !vc.is_null()
        && il2cpp_class_is_subclass_of(unsafe { (*vc).klass() }, unsafe { RR_VIEW_CLASS }, false)
        && !get_field_object_value::<Il2CppObject>(vc, unsafe { RR_RESULT_UI_FIELD }).is_null()
}

// Any missing object or failed call counts as not won.
fn rr_final_won() -> bool {
    let race_manager = RaceManager::instance();
    if race_manager.is_null() {
        return false;
    }
    let horse_manager = RaceManager::get__horseManager(race_manager);
    let race_info = RaceManager::get_RaceInfo();
    if horse_manager.is_null() || race_info.is_null() {
        return false;
    }
    let infos = RaceHorseManagerBase::GetHorseRaceInfos(horse_manager);
    if infos.is_null() {
        return false;
    }
    let infos = Array::<*mut Il2CppObject>::from(infos);
    let infos = unsafe { infos.as_slice() };
    let Ok(player) = usize::try_from(RaceHorseManagerBase::GetPlayerHorseIndex(horse_manager)) else {
        return false;
    };
    let finished = infos.get(player).is_some_and(|&p| !p.is_null() && invoke_get::<bool>(p, c"IsFinished") == Some(true));
    // get_FinishOrder is overridden (HorseRaceInfoReplay), so it goes through the object's class
    finished && race_won(
        invoke_get::<i32>(race_info, c"get_RaceType").unwrap_or(-1),
        RR_RACE_SINGLE.load(Ordering::Relaxed),
        RR_RANK_1ST.load(Ordering::Relaxed),
        player,
        infos.iter().map(|&h| if h.is_null() { None } else { invoke_get::<i32>(h, c"get_FinishOrder") })
    )
}

// Some(won) once the result screen is up and the player's horse has finished, else None.
fn rr_result() -> Option<bool> {
    // both flags are refreshed every frame before tick()
    if !RaceHorseManagerBase::is_race_active() || !HorseRaceInfo::is_finished_cached() || !rr_result_ui_up() {
        return None;
    }
    let won = rr_final_won();
    info!("superskip: race result {}", if won { "won, advancing" } else { "not won, left to the player" });
    Some(won)
}

fn rr_store(prev: Rr, next: Rr) {
    *lock(&RR) = next;
    RR_ACTIVE.store(next != Rr::Idle, Ordering::Release);
    if prev == Rr::Idle && next == Rr::Armed {
        info!("superskip: race result armed");
    } else if prev != Rr::Idle && next == Rr::Idle {
        info!("superskip: race result window closed");
    }
}

fn tick_race_result() {
    if !RR_READY.load(Ordering::Relaxed) {
        return;
    }
    let scene = SceneManager::current_scene_id() as i32;
    let (from, last) = rr_from(RR_LAST_SCENE.load(Ordering::Relaxed), scene);
    RR_LAST_SCENE.store(last, Ordering::Relaxed);
    if !RR_ACTIVE.load(Ordering::Acquire) && !rr_arms(from, scene) {
        return;
    }
    let enabled = Hachimi::instance().config.load().skip_race_result_won;
    let prev = *lock(&RR); // not held during the game calls in rr_result
    let (next, scan) = rr_step(prev, enabled, from, scene, Instant::now(), rr_result);
    rr_store(prev, next);
    RR_SCAN.store(scan, Ordering::Release);
}

fn rr_event_system() -> *mut Il2CppObject {
    let mut exc = null_mut();
    let es = il2cpp_runtime_invoke(unsafe { RR_ES_CURRENT }, null_mut(), null_mut(), &mut exc);
    if exc.is_null() { es } else { null_mut() }
}

// Scan frames only (at most every 300 ms), so the name lookup is not per frame.
fn rr_press(button: *mut Il2CppObject) {
    if button.is_null() || SceneManager::current_scene_id() != SceneId::Race
        || !Hachimi::instance().config.load().skip_race_result_won {
        return;
    }
    let name = Object::get_name(button);
    if name.is_null() {
        return;
    }
    let name = unsafe { (*name).as_utf16str() };
    if !RR_BUTTONS.iter().any(|n| name == *n) {
        return;
    }
    // locked (screen transition), under a dialog, or the dialog state unknown: leave it, the
    // next scan retries
    if invoke_get::<bool>(button, c"IsLock") != Some(false) || DialogManager::GetForeFrontDialog() != Some(null_mut()) {
        return;
    }
    let event_system = rr_event_system();
    if event_system.is_null() {
        return;
    }
    let event_data = il2cpp_object_new(unsafe { RR_POINTER_EVENT_DATA_CLASS });
    if event_data.is_null() || invoke_object_method(event_data, c".ctor", 1, &mut [event_system as _]).is_none() {
        return;
    }
    if !RR_SCAN.swap(false, Ordering::AcqRel) {
        return; // one press per scan
    }
    let name = name.to_string(); // the click may destroy the button
    if invoke_object_method(button, c"OnPointerClick", 1, &mut [event_data as _]).is_some() {
        info!("superskip: race result pressed {}", name);
    } else {
        warn!("superskip: race result press threw ({})", name);
    }
}

type ButtonUpdateFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn RaceResult_ButtonUpdate(this: *mut Il2CppObject) {
    get_orig_fn!(RaceResult_ButtonUpdate, ButtonUpdateFn)(this);
    if RR_SCAN.load(Ordering::Acquire) { // false unless armed + won, in the race scene
        rr_press(this);
    }
}

type ChangeMainViewFn = extern "C" fn(this: *mut Il2CppObject);
extern "C" fn RaceResult_ChangeMainView(this: *mut Il2CppObject) {
    if RR_ACTIVE.load(Ordering::Acquire) {
        RR_SCAN.store(false, Ordering::Release);
        let prev = *lock(&RR);
        rr_store(prev, Rr::Idle);
        info!("superskip: race result disarmed (ChangeMainView)");
    }
    get_orig_fn!(RaceResult_ChangeMainView, ChangeMainViewFn)(this);
}

// ---- init ----

fn init_training(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, SingleModeTrainingCutInHelper);

    let OnPlayCutIn_addr = get_method_addr(SingleModeTrainingCutInHelper, c"OnPlayCutIn", 0);
    let OnPlayMainCutIn_addr = get_method_addr(SingleModeTrainingCutInHelper, c"OnPlayMainCutIn", 0);

    new_hook!(OnPlayCutIn_addr, TrainingCutIn_OnPlayCutIn);
    new_hook!(OnPlayMainCutIn_addr, TrainingCutIn_OnPlayMainCutIn);
}

fn init_story(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, StoryViewController);
    get_class_or_return!(umamusume, Gallop, StoryTimelineController);

    // tick() needs all of these, so the hook stays off unless they exist
    let needed = [
        (StoryViewController, c"get_TimelineController"),
        (StoryViewController, c"get_IsPlayingOrWillPlayTrainingCutt"),
        (StoryViewController, c"SkipStory"),
        (StoryTimelineController, c"get_IsPlaying")
    ];
    if needed.iter().any(|&(class, name)| get_method(class, name, 0).is_err()) {
        error!("superskip: story members missing, story skip unavailable");
        return;
    }

    let OnStartPlayingTimeline_addr = get_method_addr(StoryViewController, c"OnStartPlayingTimeline", 0);
    new_hook!(OnStartPlayingTimeline_addr, StoryView_OnStartPlayingTimeline);
}

fn init_shop(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, PartsSingleModeScenarioFreeUseItemPerformance);

    let PlayUseItemPerformanceCoroutine_addr = get_method_addr(PartsSingleModeScenarioFreeUseItemPerformance, c"PlayUseItemPerformanceCoroutine", 6);
    new_hook!(PlayUseItemPerformanceCoroutine_addr, ShopUsePerf_PlayUseItemPerformanceCoroutine);
}

fn init_rival(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, PartsRivalEntryAnimation);
    // without the paddock class a paddock entry can't be told apart: leave the hook off
    get_class_or_return!(umamusume, Gallop, PaddockViewControllerBase);
    unsafe { PADDOCK_CLASS = PaddockViewControllerBase; }

    let PlayRivalEntryCoroutine_addr = get_method_addr(PartsRivalEntryAnimation, c"PlayRivalEntryCoroutine", 2);
    new_hook!(PlayRivalEntryCoroutine_addr, RivalEntry_PlayRivalEntryCoroutine);
}

fn init_tag(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, SingleModeMainViewTagTrainingCutInPlayer);
    unsafe { TAG_ROOT_FIELD = get_field_from_name(SingleModeMainViewTagTrainingCutInPlayer, c"_tagCutInRootObject"); }

    let PlayCutIn_addr = get_method_addr(SingleModeMainViewTagTrainingCutInPlayer, c"PlayCutIn", 2);
    let PlayCutInOut_addr = get_method_addr(SingleModeMainViewTagTrainingCutInPlayer, c"PlayCutInOut", 1);

    new_hook!(PlayCutIn_addr, TagCutIn_PlayCutIn);
    new_hook!(PlayCutInOut_addr, TagCutIn_PlayCutInOut);
}

// All or nothing: without any piece the window never arms and no button is hooked.
fn init_race_result(umamusume: *const Il2CppImage) {
    get_class_or_return!(umamusume, Gallop, ButtonCommon);
    get_class_or_return!(umamusume, Gallop, RaceMainViewController);
    get_class_or_return!(umamusume, Gallop, SingleModeChangeViewManager);
    get_class_or_return!(umamusume, Gallop, RaceDefine);
    find_nested_class_or_return!(RaceDefine, RaceType);
    get_class_or_return!(umamusume, Gallop, HorseRaceInfo);
    get_assembly_image_or_return!(ui_image, "UnityEngine.UI.dll");
    get_class_or_return!(ui_image, "UnityEngine.EventSystems", EventSystem);
    get_class_or_return!(ui_image, "UnityEngine.EventSystems", PointerEventData);

    let result_ui_field = get_field_from_name(RaceMainViewController, c"_raceResultSceneUI");
    let current = get_method(EventSystem, c"get_current", 0).unwrap_or(std::ptr::null());
    let single = get_field_from_name(RaceType, c"Single");
    let rank_1st = get_field_from_name(HorseRaceInfo, c"Rank1stNo");
    // without the dialog check a press could land under an open dialog
    if result_ui_field.is_null() || current.is_null() || single.is_null() || rank_1st.is_null()
        || !DialogManager::fore_front_dialog_available() {
        error!("superskip: race result members missing, race result skip unavailable");
        return;
    }
    // both are constants, which the runtime reads from metadata
    RR_RACE_SINGLE.store(get_static_field_value::<i32>(single), Ordering::Relaxed);
    RR_RANK_1ST.store(get_static_field_value::<i32>(rank_1st), Ordering::Relaxed);
    info!("superskip: race result ready (RaceType.Single={}, Rank1stNo={})",
        RR_RACE_SINGLE.load(Ordering::Relaxed), RR_RANK_1ST.load(Ordering::Relaxed));
    unsafe {
        RR_VIEW_CLASS = RaceMainViewController;
        RR_RESULT_UI_FIELD = result_ui_field;
        RR_ES_CURRENT = current;
        RR_POINTER_EVENT_DATA_CLASS = PointerEventData;
    }
    RR_READY.store(true, Ordering::Release);

    let ChangeMainView_addr = get_method_addr(SingleModeChangeViewManager, c"ChangeMainView", 0);
    let Update_addr = get_method_addr(ButtonCommon, c"Update", 0);
    new_hook!(ChangeMainView_addr, RaceResult_ChangeMainView);
    new_hook!(Update_addr, RaceResult_ButtonUpdate);
}

pub fn init(umamusume: *const Il2CppImage) {
    init_training(umamusume);
    init_story(umamusume);
    init_shop(umamusume);
    init_rival(umamusume);
    init_tag(umamusume);
    init_race_result(umamusume);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story_gate_rules() {
        let ms = Duration::from_millis;
        assert_eq!(story_gate(ms(0), None, false, true), Gate::Skip);
        assert_eq!(story_gate(ms(0), None, false, false), Gate::Wait, "not playing yet");
        assert_eq!(story_gate(ms(1999), None, false, false), Gate::Wait);
        assert_eq!(story_gate(ms(2001), None, false, true), Gate::Drop, "expired");
        assert_eq!(story_gate(ms(0), None, true, true), Gate::Drop, "training cutt");
        assert_eq!(story_gate(ms(0), Some(ms(50)), false, true), Gate::Drop, "debounced");
        assert_eq!(story_gate(ms(0), Some(ms(100)), false, true), Gate::Skip, "past debounce");
        assert_eq!(story_gate(ms(0), Some(ms(1400)), false, true), Gate::Skip, "after a choice");
    }

    #[test]
    fn shop_drive_and_close() {
        let mut left = 7;
        assert_eq!(drive(32, || { left -= 1; Some(left > 0) }), Drive::Done(7));
        assert_eq!(drive(32, || Some(true)), Drive::Capped);
        assert_eq!(drive(32, || None), Drive::Threw(1));

        let ms = Duration::from_millis;
        assert_eq!(shop_close_gate(ms(0), 5, 5), Gate::Wait, "same tick as the drive");
        assert_eq!(shop_close_gate(ms(16), 5, 6), Gate::Skip);
        assert_eq!(shop_close_gate(ms(2001), 5, 6), Gate::Drop, "too late");
    }

    #[test]
    fn tag_rules() {
        use Tag::*;
        // (enabled, callback_ok, out_part, in_skipped, root_hidden)
        assert_eq!(tag_action(true, true, false, false, false), Skip);
        assert_eq!(tag_action(true, false, false, false, false), Original, "nothing callable to fire");
        assert_eq!(tag_action(false, true, false, false, false), Original);
        assert_eq!(tag_action(false, true, false, false, true), RestoreThenOriginal, "toggled off after a skip");
        assert_eq!(tag_action(true, false, false, false, true), RestoreThenOriginal);
        // out part: callback_ok also covers "no callback"
        assert_eq!(tag_action(true, true, true, false, false), Skip);
        assert_eq!(tag_action(false, true, true, true, false), Skip, "finish a skipped pair, root not hidden");
        assert_eq!(tag_action(false, true, true, false, true), Original, "a hidden root alone is no skipped pair");
        assert_eq!(tag_action(false, true, true, false, false), Original);
        assert_eq!(tag_action(true, false, true, true, false), Original, "uncallable callback: the original runs");
    }

    #[test]
    fn race_result_window() {
        let (sm, race, home) = (SceneId::SingleMode as i32, SceneId::Race as i32, SceneId::Home as i32);
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let never = || -> Option<bool> { panic!("decided outside the armed state") };

        // arming: only on SingleMode -> Race, only when enabled
        assert_eq!(rr_step(Rr::Idle, true, sm, race, t0, never), (Rr::Armed, false));
        assert_eq!(rr_step(Rr::Idle, false, sm, race, t0, never), (Rr::Idle, false), "toggle off");
        assert_eq!(rr_step(Rr::Idle, true, race, race, t0, never), (Rr::Idle, false), "toggled on mid-race");
        for other in [home, SceneId::TeamStadium as i32, SceneId::DailyRace as i32, SceneId::RoomMatch as i32] {
            assert_eq!(rr_step(Rr::Idle, true, other, race, t0, never), (Rr::Idle, false));
        }
        assert_eq!(rr_step(Rr::Idle, true, sm, SceneId::TeamStadium as i32, t0, never), (Rr::Idle, false));

        // armed: wait for the result, a loss closes the window
        assert_eq!(rr_step(Rr::Armed, true, race, race, t0, || None), (Rr::Armed, false));
        assert_eq!(rr_step(Rr::Armed, true, race, race, t0, || Some(false)), (Rr::Idle, false));
        assert_eq!(rr_step(Rr::Armed, true, race, sm, t0, never), (Rr::Idle, false), "left the race scene");
        assert_eq!(rr_step(Rr::Armed, false, race, race, t0, never), (Rr::Idle, false));
        let won = Rr::Won { until: t0 + RR_WINDOW, next_scan: t0 };
        assert_eq!(rr_step(Rr::Armed, true, race, race, t0, || Some(true)), (won, false));

        // won: one scan per 300 ms, closed by scene exit, toggle off or 90 s
        let (after, scan) = rr_step(won, true, race, race, t0, never);
        assert!(scan);
        assert_eq!(rr_step(after, true, race, race, t0 + ms(299), never), (after, false), "rate limit");
        assert!(rr_step(after, true, race, race, t0 + ms(300), never).1);
        assert_eq!(rr_step(won, true, race, sm, t0, never), (Rr::Idle, false));
        assert_eq!(rr_step(won, false, race, race, t0, never), (Rr::Idle, false));
        assert_eq!(rr_step(won, true, race, race, t0 + RR_WINDOW, never), (Rr::Idle, false), "timeout");
        assert_eq!(rr_step(Rr::Idle, true, race, race, t0, never), (Rr::Idle, false), "no re-arm in the same race");

        // loading frames: the scene we came from is kept, and an armed/won state holds unpressed
        let none = SceneId::None as i32;
        assert_eq!(rr_from(sm, none), (sm, sm));
        assert_eq!(rr_from(sm, none), (sm, sm));
        assert_eq!(rr_from(sm, race), (sm, race), "SingleMode -> None -> Race arms");
        assert_eq!(rr_from(race, race), (race, race), "next Race tick");
        assert_eq!(rr_step(Rr::Armed, true, race, none, t0, never), (Rr::Armed, false));
        assert_eq!(rr_step(after, true, race, none, t0 + ms(300), never), (after, false));
    }

    #[test]
    fn race_won_rules() {
        let o = |v: &[i32]| v.iter().map(|&x| Some(x)).collect::<Vec<_>>();
        let won = |race_type, player, orders| race_won(race_type, 6, 0, player, orders); // Global values
        assert!(won(6, 1, o(&[2, 0, 1])));
        assert!(!won(6, 1, o(&[0, 1, 2])), "2nd place");
        assert!(!won(7, 1, o(&[2, 0, 1])), "team race");
        assert!(!won(6, 0, o(&[0, 0, 0])), "orders not set");
        assert!(!won(6, 0, vec![Some(0), None]), "a call failed");
        assert!(!won(6, 5, o(&[0, 1])), "player not in the list");
        assert!(!race_won(6, -1, -1, 1, o(&[2, 0, 1])), "constants not resolved");
    }

    #[test]
    fn old_configs_get_skip_defaults() {
        let c: crate::core::hachimi::Config = serde_json::from_str(r#"{"story_tcps_multiplier": 2.0}"#).unwrap();
        assert!(c.skip_training_cutin);
        assert!(!(c.skip_story_events || c.skip_story_everywhere || c.skip_shop_performance
            || c.skip_rival_entry || c.skip_tag_cutin || c.skip_race_result_won));
    }
}
