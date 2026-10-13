//! Prepared, bounded app source into the existing Engine. This owns no provider
//! credentials, transport, CDM, metadata matcher, or reporting session.
use super::*;
use std::ffi::CString;
use std::os::raw::c_void;

/// Prototype limit: caller supplies validated H264/AAC/SVP Load facts and
/// already prepared output. Duration is capped to the accepted 30-second probe.
/// This is not the eventual streaming-source API or validation of a DASH URL.
pub struct AppSourceStart {
    pub generation: u64,
    pub payload: CString,
    pub duration_ns: i64,
    pub raster: (i32, i32),
}
#[derive(Debug, PartialEq, Eq)]
pub enum AppSourceError { Invalid, Busy, NativeBusy, StartFailed }
#[derive(Debug, PartialEq, Eq)]
pub enum AppEnqueue { Accepted, Full, Invalid, Stale }
#[derive(Debug, PartialEq, Eq)]
pub enum AppSourceStatus { Idle, Loading, Buffering, Playing, Ended, Failed }

/// Uses the same native clock/start guards, Load worker and Engine initializer
/// as the Plex path. The app owns the selected source and all provider effects.
pub fn start_app_source(pa: &mut super::super::adapter::PlayerAdapter, start: AppSourceStart)
    -> Result<u32, AppSourceError> {
    if start.generation == 0 || !(1..=30_000_000_000).contains(&start.duration_ns)
        || !(1..=4096).contains(&start.raster.0) || !(1..=2160).contains(&start.raster.1)
        || start.payload.to_str().is_err() || start.payload.as_bytes().len() > 16_384 {
        return Err(AppSourceError::Invalid);
    }
    // Refuse malformed Load objects before acquiring any native/window resources.
    let Ok(mut load) = serde_json::from_slice::<serde_json::Value>(start.payload.as_bytes()) else {
        return Err(AppSourceError::Invalid);
    };
    let Some(option) = load.pointer_mut("/args/0/option").and_then(|v| v.as_object_mut()) else {
        return Err(AppSourceError::Invalid);
    };
    // Compact JSON gives the shared window splicer an exact appId anchor.
    // Preserve this app's identity and codec/SVP facts, independently of Plex.
    if option.contains_key("windowId") { return Err(AppSourceError::Invalid); }
    let Some(app_id) = option.get("appId").and_then(|v| v.as_str()) else {
        return Err(AppSourceError::Invalid);
    };
    if app_id.is_empty() || app_id.len() > 128 { return Err(AppSourceError::Invalid); }
    let app_id = app_id.to_owned();
    let composed = serde_json::to_string(&load).map_err(|_| AppSourceError::Invalid)?;
    if pa.is_live() { return Err(AppSourceError::Busy); }
    reap_abandoned_load(pa);
    if pa.has_abandoned_load() || plx_platform::tv::sandbox::blocks_native_video() {
        return Err(AppSourceError::NativeBusy);
    }
    let clock = ClockStartGuard::arm().ok_or(AppSourceError::NativeBusy)?;
    let native = NativeSessionStartGuard::arm().ok_or(AppSourceError::NativeBusy)?;
    let epoch = native.epoch();
    if !pa.admit_app_generation(start.generation) { return Err(AppSourceError::Invalid); }
    let payload = CString::new(with_window_id_for_app(pa.mt(), &composed, &app_id))
        .map_err(|_| AppSourceError::Invalid)?;
    if sink().window_mode() == super::super::VP_EXPORTED
        && !serde_json::from_slice::<serde_json::Value>(payload.as_bytes())
            .is_ok_and(|v| v.pointer("/args/0/option/windowId").is_some_and(|id| id.as_str().is_some_and(|id| !id.is_empty()))) {
        release_native_object(pa.mt(), epoch);
        return Err(AppSourceError::StartFailed);
    }
    SHARED.duration_ns.store(start.duration_ns, Ordering::Relaxed);
    SHARED.publish_video_raster(start.raster.0, start.raster.1);
    SHARED.hls_audio_expected.store(true, Ordering::Release);
    VTOT.store(0, Ordering::Relaxed); ATOT.store(0, Ordering::Relaxed);
    VATT.store(0, Ordering::Relaxed); AATT.store(0, Ordering::Relaxed);
    let pointer = threads::SendPtr(payload.as_ptr() as *mut c_char);
    let load = plx_base::task::spawn("media", move || threads::load_thread(pointer, epoch, None));
    let Some(load) = load else {
        release_native_object(pa.mt(), epoch);
        SHARED.reset_session(); TX.reset();
        return Err(AppSourceError::StartFailed);
    };
    let mut eng = Engine::new(None, epoch, payload, Source::AppOwned { generation: start.generation },
        plx_net::stream::http_stream_boxed());
    eng.aq_video = Some(crate::aq::aq_new(AQ_VIDEO_BYTES));
    eng.aq_audio = Some(crate::aq::aq_new(AQ_AUDIO_BYTES));
    eng.load_th = Some(load);
    // The external adapter has no private in-place-seek policy. Seeking is
    // deliberately absent from this bounded integration entry.
    super::super::INPLACE_SEEK_OK.store(false, Ordering::Relaxed);
    pa.install(eng); native.commit(); clock.commit();
    TX.started.store(true, Ordering::Relaxed);
    Ok(epoch)
}

fn owns(eng: &Engine, generation: u64) -> bool {
    matches!(eng.source, Source::AppOwned { generation: current } if generation != 0 && current == generation)
}

/// Only Accepted transfers owner; every other result leaves it with the caller.
/// # Safety
/// Called serially on pa's SDL thread. data/owner remain live until release;
/// release must run on that same thread and must not unwind/reenter. The app
/// matches/decrypts the original CENC sample before admitting this opaque output.
#[allow(clippy::too_many_arguments)]
pub unsafe fn app_source_enqueue(pa: &mut super::super::adapter::PlayerAdapter,
    generation: u64, lane: c_int, key: c_int, pts_ns: i64, data: *const u8, len: c_int,
    owner: *mut c_void, release: Option<unsafe extern "C" fn(*mut c_void)>) -> AppEnqueue {
    let Some(eng) = pa.engine() else { return AppEnqueue::Stale; };
    if !owns(eng,generation) { return AppEnqueue::Stale; }
    if !(1..=2).contains(&lane) || !(1..=1024*1024).contains(&len)
        || pts_ns < 0 || SHARED.demux_io_failed.load(Ordering::Acquire)
        || SHARED.load_failed.load(Ordering::Acquire) { return AppEnqueue::Invalid; }
    let q = if lane == 1 { &mut eng.aq_video } else { &mut eng.aq_audio };
    let Some(q) = q.as_mut() else { return AppEnqueue::Invalid; };
    match crate::aq::aq_try_push_owned(&mut **q, data, len, pts_ns, key, lane, owner, release) {
        0 => AppEnqueue::Accepted, 1 => AppEnqueue::Full, _ => AppEnqueue::Invalid,
    }
}

pub fn app_source_eof(pa: &mut super::super::adapter::PlayerAdapter, generation: u64, lane: c_int) -> bool {
    let Some(eng) = pa.engine() else { return false; };
    if !owns(eng,generation) || !(1..=2).contains(&lane) { return false; }
    let q = if lane == 1 { &mut eng.aq_video } else { &mut eng.aq_audio };
    let Some(q) = q.as_mut() else { return false; };
    crate::aq::aq_set_eof(&mut **q); true
}

/// Same native stage transitions, dual-lane feed, pacing, prime and EOS handling.
/// No route pump, Plex control flight, reporter, or route recovery is entered.
pub fn pump_app_source(pa: &mut super::super::adapter::PlayerAdapter, now: u32) -> AppSourceStatus {
    // Stop can leave Load in flight. Keep reaping its return on the main thread,
    // even when there is no live Engine and the caller is showing its catalog.
    reap_abandoned_load(pa);
    let (eng,mt) = pa.split();
    let Some(eng) = eng else { return AppSourceStatus::Idle; };
    if !matches!(eng.source, Source::AppOwned { .. }) { return AppSourceStatus::Idle; }
    if SHARED.load_failed.load(Ordering::Acquire) || SHARED.demux_io_failed.load(Ordering::Acquire) {
        return AppSourceStatus::Failed;
    }
    // No other native verb before Load returns, including loadCompleted polling.
    if eng.stage == Stage::Loading && !super::super::pump::native_load_gate_ready(eng.native_epoch, false) {
        return if SHARED.load_failed.load(Ordering::Acquire) { AppSourceStatus::Failed } else { AppSourceStatus::Loading };
    }
    super::super::pump::pump_native_stages(mt, eng, now, true, false, super::super::pump::NativeLoadReadiness::ReturnedForPreroll);
    if SHARED.load_failed.load(Ordering::Acquire) { return AppSourceStatus::Failed; }
    if eng.stage < Stage::Playing { return AppSourceStatus::Loading; }
    if TX.feed_allowed() { feed_both_lanes(mt,eng); }
    if SHARED.demux_io_failed.load(Ordering::Acquire) { return AppSourceStatus::Failed; }
    if eng.eos_pushed && (SHARED.ended.load(Ordering::Relaxed)
        || SHARED.playpos_ns.load(Ordering::Relaxed) >= SHARED.duration_ns.load(Ordering::Relaxed) - 1_000_000_000) {
        AppSourceStatus::Ended
    } else if eng.prime_play || SHARED.frames.load(Ordering::Relaxed) == 0 {
        AppSourceStatus::Buffering
    } else { AppSourceStatus::Playing }
}

/// Exact generation owns cancellation. Native cleanup is shared with Plex; an
/// in-flight Load is parked immediately rather than blocking the SDL thread.
/// Returning confirms ownership cancellation, not completion of a parked Load.
/// Keep the sink, adapter and native/CDM resources alive until that Load is reaped;
/// then release the independently owned CDM/provider under the app lifecycle.
pub fn stop_app_source(pa: &mut super::super::adapter::PlayerAdapter, generation: u64) -> bool {
    if !pa.engine().is_some_and(|eng| owns(eng,generation)) { return false; }
    let mut eng = pa.take().expect("main-thread checked owner");
    teardown_native_engine(pa,&mut eng,true, |_| {});
    SHARED.reset_session(); TX.reset();
    true
}
