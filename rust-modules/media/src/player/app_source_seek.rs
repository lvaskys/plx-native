//! Generation-scoped app-owned queue handoff, through the shared seek machinery.
//! This low-level contract admits window-local CDM/opaque timestamps starting
//! at zero. The embedding source still owns episode target/index/time mapping.
use super::*;
use crate::player::adapter::PlayerAdapter;

pub(super) fn input_blocked(eng: &mut Engine) -> bool {
    eng.app_seek
        .as_ref()
        .is_some_and(|s| s.phase != AppSeekPhase::Feeding)
}
pub(super) fn pump_guard(eng: &mut Engine) -> Option<AppSourceStatus> {
    let s = eng.app_seek.as_mut()?;
    if s.phase == AppSeekPhase::Feeding && eng.flushed && crate::player::native_seek_refused() {
        s.phase = AppSeekPhase::ReloadRequired;
    }
    match s.phase {
        AppSeekPhase::Preparing => Some(if TX.paused.load(Ordering::Acquire) {
            AppSourceStatus::Paused
        } else {
            AppSourceStatus::Buffering
        }),
        AppSeekPhase::ReloadRequired => Some(AppSourceStatus::Failed),
        AppSeekPhase::Feeding => None,
    }
}

/// Reserve a never-reused queue generation and hold the existing native clock.
/// Caller must stop its producer before this call; queue disposal is owner-thread
/// only. Begin does not claim that the embedding input workers have retired.
pub fn begin_app_source_seek(
    pa: &mut PlayerAdapter,
    generation: u64,
    next: u64,
) -> Result<AppSourceSeekTicket, AppSourceSeekError> {
    let eng = pa.engine().ok_or(AppSourceSeekError::Invalid)?;
    if !owns(eng, generation) || next <= generation {
        return Err(AppSourceSeekError::Invalid);
    }
    if eng
        .app_seek
        .as_ref()
        .is_some_and(|s| s.phase != AppSeekPhase::Feeding)
        || eng.stage < Stage::Playing
        || eng.prime_play
        || TX.seek_preroll_active()
        || eng.rebase_pending
        || !SHARED.native_load_returned(eng.native_epoch)
        || !SHARED.load_completed.load(Ordering::Acquire)
        || SHARED.frames.load(Ordering::Acquire) <= 0
        || SHARED.load_failed.load(Ordering::Acquire)
        || SHARED.demux_io_failed.load(Ordering::Acquire)
        || SHARED.ended.load(Ordering::Acquire)
    {
        return Err(AppSourceSeekError::NotReady);
    }
    if sink().window_mode() != crate::player::VP_EXPORTED || crate::player::native_seek_refused() {
        return Err(AppSourceSeekError::ReloadRequired);
    }
    let ticket = AppSourceSeekTicket {
        epoch: eng.native_epoch,
        previous: generation,
        next,
        paused: TX.paused.load(Ordering::Acquire),
    };
    if !pa.admit_app_generation(next) {
        return Err(AppSourceSeekError::Invalid);
    }
    let (eng, mt) = pa.split();
    let eng = eng.expect("validated serial Engine");
    eng.app_seek = Some(AppSeekTransaction {
        ticket,
        phase: AppSeekPhase::Preparing,
        first_video: true,
        first_video_pts_ns: 0,
    });
    if hold_native_clock_for_seek(mt) != Some(crate::player::shared::HlsPauseCompletion::Accepted)
        || !SHARED.begin_native_media_discontinuity(ticket.epoch)
    {
        eng.app_seek.as_mut().unwrap().phase = AppSeekPhase::ReloadRequired;
        return Err(AppSourceSeekError::ReloadRequired);
    }
    // Preserve the orthogonal viewer hold. Once committed, the shared paused
    // seek preroll borrows it to present a muted frame and then re-pauses.
    if ticket.paused
        && !matches!(
            SHARED.prepare_hls_user_pause(),
            Some(crate::player::shared::HlsUserPause::AlreadyHeld)
        )
    {
        eng.app_seek.as_mut().unwrap().phase = AppSeekPhase::ReloadRequired;
        return Err(AppSourceSeekError::ReloadRequired);
    }
    drain_aq(eng);
    // There is no demux worker holding these app-owned queues. Replace them so
    // a previous source's EOF/abort/headroom cannot survive into the next one.
    for queue in [&mut eng.aq_video, &mut eng.aq_audio] {
        if let Some(mut old) = queue.take() {
            crate::aq::aq_destroy(&mut *old);
        }
    }
    eng.aq_video = Some(crate::aq::aq_new(AQ_VIDEO_BYTES));
    eng.aq_audio = Some(crate::aq::aq_new(AQ_AUDIO_BYTES));
    Ok(ticket)
}

/// Install a validated remaining window. Its first video AU MUST be a keyframe
/// at local PTS zero; enqueue enforces this without taking a refused owner.
/// The existing keyframe Feed path calls public Seek(0), exactly once, before
/// new media. It never rewrites opaque bytes, calls private Segment or flushes.
pub fn commit_app_source_seek(
    pa: &mut PlayerAdapter,
    ticket: AppSourceSeekTicket,
    duration_ns: i64,
) -> Result<(), AppSourceSeekError> {
    commit_app_source_seek_at(pa, ticket, duration_ns, 0)
}

/// Preserve a validated local video composition offset (for example 83ms after
/// window decode origin). The embedding maps this through the existing CDM
/// microsecond conversion; no opaque timestamp is rewritten. Native Seek uses
/// the same first keyframe PTS through the shared Feed path.
pub fn commit_app_source_seek_at(
    pa: &mut PlayerAdapter,
    ticket: AppSourceSeekTicket,
    duration_ns: i64,
    first_video_pts_ns: i64,
) -> Result<(), AppSourceSeekError> {
    if !(0..=500_000_000).contains(&first_video_pts_ns) || first_video_pts_ns >= duration_ns {
        return Err(AppSourceSeekError::Invalid);
    }
    if !(1..=3_600_000_000_000).contains(&duration_ns) {
        return Err(AppSourceSeekError::Invalid);
    }
    let eng = pa.engine().ok_or(AppSourceSeekError::Invalid)?;
    if eng.native_epoch != ticket.epoch
        || !owns(eng, ticket.previous)
        || !eng
            .app_seek
            .as_ref()
            .is_some_and(|s| s.ticket == ticket && s.phase == AppSeekPhase::Preparing)
    {
        return Err(AppSourceSeekError::Invalid);
    }
    if TX.paused.load(Ordering::Acquire) != ticket.paused
        || SHARED.load_failed.load(Ordering::Acquire)
        || SHARED.demux_io_failed.load(Ordering::Acquire)
    {
        eng.app_seek.as_mut().unwrap().phase = AppSeekPhase::ReloadRequired;
        return Err(AppSourceSeekError::ReloadRequired);
    }
    reset_seek_feed_state(eng, 0, crate::player::vclock_ms());
    eng.eos_pushed = false;
    eng.source = Source::AppOwned {
        generation: ticket.next,
    };
    eng.app_seek.as_mut().unwrap().phase = AppSeekPhase::Feeding;
    eng.app_seek.as_mut().unwrap().first_video_pts_ns = first_video_pts_ns;
    SHARED.duration_ns.store(duration_ns, Ordering::Relaxed);
    SHARED.ended.store(false, Ordering::Release);
    SHARED.seek_target_ns.store(first_video_pts_ns, Ordering::Relaxed);
    SHARED.disp_base.store(0, Ordering::Relaxed);
    SHARED.hls_video_tail_ns.store(-1, Ordering::Relaxed);
    SHARED.hls_audio_tail_ns.store(-1, Ordering::Relaxed);
    VTOT.store(0, Ordering::Relaxed);
    ATOT.store(0, Ordering::Relaxed);
    VATT.store(0, Ordering::Relaxed);
    AATT.store(0, Ordering::Relaxed);
    APP_STARTED.store(crate::player::vclock_ms(), Ordering::Release);
    APP_LOAD_COMPLETED.store(-1, Ordering::Release);
    APP_FIRST_PICTURE.store(-1, Ordering::Release);
    APP_PLAY_ATTEMPTS.store(0, Ordering::Release);
    APP_PLAY_ACKNOWLEDGED.store(0, Ordering::Release);
    APP_FIRST_PLAY.store(-1, Ordering::Release);
    APP_LAST_PLAY.store(-1, Ordering::Release);
    crate::player::set_seek_mode(crate::player::SeekMode::NativeSeek);
    Ok(())
}

/// Once begin has retired queued owners, abort cannot resume the old source.
/// It fences admission until real Stop/reload, without fabricating retirement.
pub fn abort_app_source_seek(pa: &mut PlayerAdapter, ticket: AppSourceSeekTicket) -> bool {
    let Some(eng) = pa.engine() else {
        return false;
    };
    let Some(s) = eng.app_seek.as_mut() else {
        return false;
    };
    if eng.native_epoch != ticket.epoch || s.ticket != ticket {
        return false;
    }
    s.phase = AppSeekPhase::ReloadRequired;
    true
}
pub fn app_source_seek_status(
    pa: &mut PlayerAdapter,
    ticket: AppSourceSeekTicket,
) -> AppSourceSeekStatus {
    let Some(eng) = pa.engine() else {
        return AppSourceSeekStatus::Stale;
    };
    let Some(s) = eng.app_seek.as_ref() else {
        return AppSourceSeekStatus::Stale;
    };
    if eng.native_epoch != ticket.epoch || s.ticket != ticket {
        return AppSourceSeekStatus::Stale;
    }
    if SHARED.load_failed.load(Ordering::Acquire) || SHARED.demux_io_failed.load(Ordering::Acquire)
    {
        return AppSourceSeekStatus::Failed;
    }
    match s.phase {
        AppSeekPhase::Preparing => AppSourceSeekStatus::Preparing,
        AppSeekPhase::ReloadRequired => AppSourceSeekStatus::ReloadRequired,
        AppSeekPhase::Feeding => {
            if eng.flushed && crate::player::native_seek_refused() {
                AppSourceSeekStatus::ReloadRequired
            } else if eng.prime_play
                || TX.seek_preroll_active()
                || eng.rebase_pending
                || SHARED.frames.load(Ordering::Acquire) == 0
            {
                AppSourceSeekStatus::Priming
            } else {
                AppSourceSeekStatus::Ready
            }
        }
    }
}
