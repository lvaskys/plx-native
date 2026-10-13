//! Exercises the actual upstream engine, not a replacement feed loop.
use super::*;
use std::os::raw::c_void;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use super::super::ffi_host;
struct Owner { bytes: Vec<u8>, released: Rc<Cell<u32>> }
unsafe extern "C" fn release(p: *mut c_void) {
    let o = Box::from_raw(p.cast::<Owner>());
    o.released.set(o.released.get()+1);
}
fn fresh() -> super::super::adapter::PlayerAdapter {
    SHARED.reset_session(); TX.reset();
    ffi_host::reset_native_lifecycle_for_test();
    ffi_host::force_clocksink_for_test(true);
    super::super::adapter::PlayerAdapter::new(unsafe { MainThread::assume() })
}
fn config(generation: u64) -> AppSourceStart {
    AppSourceStart { generation, payload: std::ffi::CString::new(r#"{"args":[{"option":{"appId":"com.linas.nativeaccountprobe","externalStreamingInfo":{"svpVersion":30},"drm":{"type":"WIDEVINE_MODULAR"}}}]}"#).unwrap(),
        duration_ns: 30_000_000_000, raster: (1920,1080) }
}
fn ready(pa: &mut super::super::adapter::PlayerAdapter) {
    let since = Instant::now();
    while pa.engine().unwrap().load_th.as_ref().is_some_and(|t| !t.is_finished()) {
        assert!(since.elapsed() < Duration::from_secs(2)); std::thread::yield_now();
    }
    pump_app_source(pa, super::super::vclock_ms());
    assert!(pa.engine().unwrap().stage >= Stage::Playing);
}
fn enqueue(pa: &mut super::super::adapter::PlayerAdapter, generation: u64, lane: i32,
    pts: i64, count: &Rc<Cell<u32>>) -> (*const u8, AppEnqueue) {
    let o = Box::new(Owner { bytes: vec![0xa5;5], released: count.clone() });
    let data = o.bytes.as_ptr();
    let owner = Box::into_raw(o).cast();
    let status = unsafe { app_source_enqueue(pa, generation, lane, 1, pts, data, 5, owner, Some(release)) };
    if status != AppEnqueue::Accepted { unsafe { release(owner) }; }
    (data, status)
}
#[test]
fn app_source_owns_native_engine_without_a_plex_route_and_can_repeat() {
    let _g = plx_base::testlock::serial();
    let phase = crate::route::control_phase_label();
    let mut pa = fresh();
    let first = start_app_source(&mut pa, config(1)).unwrap();
    ready(&mut pa);
    assert!(pa.engine().unwrap().route_start.is_none());
    assert!(pa.engine().unwrap().report_th.is_none());
    assert_eq!(crate::route::control_phase_label(), phase);
    assert!(stop_app_source(&mut pa,1));
    let next = start_app_source(&mut pa, config(2)).unwrap();
    assert_ne!(first,next);
    ready(&mut pa);
    assert!(stop_app_source(&mut pa,2));
    assert_eq!(crate::route::control_phase_label(), phase);
    ffi_host::force_clocksink_for_test(false);
}
#[test]
fn real_feed_retries_the_same_owned_block_and_keeps_audio_independent() {
    let _g = plx_base::testlock::serial();
    let mut pa = fresh();
    start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    let v = Rc::new(Cell::new(0)); let a = Rc::new(Cell::new(0));
    let (vp,_) = enqueue(&mut pa,1,1,700_000_000,&v);
    let (ap,_) = enqueue(&mut pa,1,2,400_000_000,&a);
    ffi_host::owned_feed_script_for_test(vec![b'B',b'O',b'O']);
    pump_app_source(&mut pa,super::super::vclock_ms());
    assert_eq!((v.get(),a.get()),(0,1));
    assert_eq!(pa.engine().unwrap().max_fed_video_pts,0);
    assert_eq!(pa.engine().unwrap().max_fed_audio_pts,400_000_000);
    pump_app_source(&mut pa,super::super::vclock_ms());
    assert_eq!((v.get(),a.get()),(1,1));
    assert_eq!(ffi_host::take_owned_feeds_for_test(), vec![(vp as usize,5,700_000_000,1),
        (ap as usize,5,400_000_000,2),(vp as usize,5,700_000_000,1)]);
    assert!(stop_app_source(&mut pa,1));
    ffi_host::force_clocksink_for_test(false);
}

#[test]
fn stop_drains_pending_and_queued_owners_and_rejects_stale_work() {
    let _g = plx_base::testlock::serial();
    let mut pa = fresh(); start_app_source(&mut pa,config(7)).unwrap(); ready(&mut pa);
    let count = Rc::new(Cell::new(0));
    enqueue(&mut pa,7,1,0,&count); enqueue(&mut pa,7,1,1,&count);
    enqueue(&mut pa,7,2,0,&count); enqueue(&mut pa,7,2,1,&count);
    ffi_host::owned_feed_script_for_test(vec![b'B',b'B']);
    pump_app_source(&mut pa,super::super::vclock_ms());
    assert_eq!(count.get(),0);
    assert!(!stop_app_source(&mut pa,6));
    assert!(stop_app_source(&mut pa,7));
    assert_eq!(count.get(),4);
    ffi_host::take_owned_feeds_for_test();
    start_app_source(&mut pa,config(8)).unwrap(); ready(&mut pa);
    assert_eq!(enqueue(&mut pa,7,1,0,&count).1,AppEnqueue::Stale);
    assert_eq!(count.get(),5);
    assert!(stop_app_source(&mut pa,8));
    assert!(matches!(start_app_source(&mut pa,config(8)),Err(AppSourceError::Invalid)));
    ffi_host::force_clocksink_for_test(false);
}
#[test]
fn terminal_feed_error_releases_pending_and_does_not_advance_or_prime() {
    let _g = plx_base::testlock::serial();
    let mut pa = fresh(); start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    let count = Rc::new(Cell::new(0));
    enqueue(&mut pa,1,1,700_000_000,&count); enqueue(&mut pa,1,2,400_000_000,&count);
    ffi_host::owned_feed_script_for_test(vec![b'e']);
    let plays = ffi_host::play_calls_for_test();
    assert_eq!(pump_app_source(&mut pa,super::super::vclock_ms()),AppSourceStatus::Failed);
    assert_eq!(count.get(),1);
    assert_eq!(pa.engine().unwrap().max_fed_video_pts,0);
    assert_eq!(ffi_host::play_calls_for_test(),plays);
    assert_eq!(ffi_host::take_owned_feeds_for_test().len(),1);
    assert!(stop_app_source(&mut pa,1)); assert_eq!(count.get(),2);
    ffi_host::force_clocksink_for_test(false);
}
#[test]
fn eos_waits_for_both_closed_lanes_and_retained_audio() {
    let _g = plx_base::testlock::serial();
    let mut pa = fresh(); start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    let count = Rc::new(Cell::new(0));
    enqueue(&mut pa,1,1,700_000_000,&count); enqueue(&mut pa,1,2,400_000_000,&count);
    assert!(app_source_eof(&mut pa,1,1));
    ffi_host::owned_feed_script_for_test(vec![b'O',b'B',b'O']);
    pump_app_source(&mut pa,super::super::vclock_ms());
    assert!(!pa.engine().unwrap().eos_pushed);
    assert!(app_source_eof(&mut pa,1,2));
    pump_app_source(&mut pa,super::super::vclock_ms());
    assert!(!pa.engine().unwrap().eos_pushed);
    pump_app_source(&mut pa,super::super::vclock_ms());
    assert!(pa.engine().unwrap().eos_pushed);
    assert_eq!(count.get(),2);
    ffi_host::take_owned_feeds_for_test();
    assert!(stop_app_source(&mut pa,1));
    ffi_host::force_clocksink_for_test(false);
}
#[test]
fn native_callbacks_from_a_retired_source_cannot_update_its_replacement() {
    let _g = plx_base::testlock::serial();
    let mut pa = fresh(); let first=start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    assert!(stop_app_source(&mut pa,1));
    let next=start_app_source(&mut pa,config(2)).unwrap(); ready(&mut pa);
    super::super::sf_on_event(first,0,9_000_000_000,std::ptr::null());
    assert_eq!(SHARED.playpos_ns.load(Ordering::Relaxed),0);
    super::super::sf_on_event(next,0,4_000_000_000,std::ptr::null());
    assert_eq!(SHARED.playpos_ns.load(Ordering::Relaxed),4_000_000_000);
    assert!(stop_app_source(&mut pa,2)); ffi_host::force_clocksink_for_test(false);
}
#[test]
fn stop_during_load_parks_payload_and_blocks_replacement_until_return() {
    let _g = plx_base::testlock::serial();
    let mut pa=fresh(); ffi_host::hold_load_for_test();
    start_app_source(&mut pa,config(1)).unwrap();
    let since=Instant::now();
    while !ffi_host::load_in_flight_for_test() {
        assert!(since.elapsed()<Duration::from_secs(2)); std::thread::yield_now();
    }
    for _ in 0..5 { assert_eq!(pump_app_source(&mut pa,super::super::vclock_ms()),AppSourceStatus::Loading); }
    assert_eq!(ffi_host::in_flight_calls_for_test(),0);
    let count=Rc::new(Cell::new(0)); enqueue(&mut pa,1,1,0,&count);
    let (done, wait) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if wait.recv_timeout(Duration::from_secs(2)).is_err() { ffi_host::release_load_for_test(); }
    });
    assert!(stop_app_source(&mut pa,1));
    done.send(()).unwrap(); watchdog.join().unwrap();
    assert!(ffi_host::load_in_flight_for_test());
    assert_eq!(count.get(),1); assert!(pa.has_abandoned_load());
    assert_eq!(ffi_host::in_flight_calls_for_test(),0);
    assert!(matches!(start_app_source(&mut pa,config(2)),Err(AppSourceError::NativeBusy)));
    ffi_host::release_load_for_test();
    let since=Instant::now();
    while pa.has_abandoned_load() {
        assert_eq!(pump_app_source(&mut pa,super::super::vclock_ms()),AppSourceStatus::Idle);
        assert!(since.elapsed()<Duration::from_secs(2)); std::thread::yield_now();
    }
    start_app_source(&mut pa,config(2)).unwrap(); ready(&mut pa);
    assert!(stop_app_source(&mut pa,2)); ffi_host::force_clocksink_for_test(false);
}
#[test]
fn invalid_configuration_and_overlapping_start_leave_the_live_owner_intact() {
    let _g = plx_base::testlock::serial(); let mut pa=fresh();
    assert!(matches!(start_app_source(&mut pa,config(0)),Err(AppSourceError::Invalid)));
    let mut bad=config(1); bad.payload=std::ffi::CString::new("[]").unwrap();
    assert!(matches!(start_app_source(&mut pa,bad),Err(AppSourceError::Invalid)));
    let epoch=start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    assert!(matches!(start_app_source(&mut pa,config(2)),Err(AppSourceError::Busy)));
    assert_eq!(pa.engine().unwrap().native_epoch,epoch);
    assert!(stop_app_source(&mut pa,1)); ffi_host::force_clocksink_for_test(false);
}

#[test]
fn accepted_load_can_feed_preroll_without_a_loadcompleted_callback_and_preserves_svp() {
    let _g = plx_base::testlock::serial(); let mut pa=fresh();
    ffi_host::suppress_app_completed_for_test(true);
    start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    assert!(!SHARED.load_completed.load(Ordering::Relaxed));
    let payload=serde_json::from_slice::<serde_json::Value>(pa.engine().unwrap().payload.as_bytes()).unwrap();
    assert_eq!(payload.pointer("/args/0/option/externalStreamingInfo/svpVersion").unwrap(),30);
    assert_eq!(payload.pointer("/args/0/option/drm/type").unwrap(),"WIDEVINE_MODULAR");
    assert_eq!(payload.pointer("/args/0/option/windowId").unwrap(),"clocksink-window");
    assert_eq!(payload.pointer("/args/0/option/appId").unwrap(),"com.linas.nativeaccountprobe");
    let count=Rc::new(Cell::new(0)); enqueue(&mut pa,1,1,700_000_000,&count); enqueue(&mut pa,1,2,400_000_000,&count);
    ffi_host::owned_feed_script_for_test(vec![b'O',b'O']);
    pump_app_source(&mut pa,super::super::vclock_ms()); assert_eq!(count.get(),2);
    ffi_host::take_owned_feeds_for_test();
    assert!(stop_app_source(&mut pa,1)); ffi_host::force_clocksink_for_test(false);
}

#[test]
fn terminal_video_error_cannot_release_an_already_buffered_prime() {
    let _g = plx_base::testlock::serial();
    let mut pa=fresh(); start_app_source(&mut pa,config(1)).unwrap(); ready(&mut pa);
    // Accepted runway exists, but Play is still held. A terminal Feed response
    // must fail the run before the in-loop prime check can release that hold.
    let eng=pa.engine().unwrap();
    eng.max_fed_video_pts=700_000_000;
    eng.max_fed_audio_pts=400_000_000;
    assert!(eng.prime_play);
    let count=Rc::new(Cell::new(0)); enqueue(&mut pa,1,1,800_000_000,&count);
    ffi_host::owned_feed_script_for_test(vec![b'e']);
    let plays=ffi_host::play_calls_for_test();
    assert_eq!(pump_app_source(&mut pa,super::super::vclock_ms()),AppSourceStatus::Failed);
    assert_eq!(ffi_host::play_calls_for_test(),plays);
    assert_eq!(count.get(),1);
    assert_eq!(pa.engine().unwrap().max_fed_video_pts,700_000_000);
    assert!(stop_app_source(&mut pa,1)); ffi_host::force_clocksink_for_test(false);
}
