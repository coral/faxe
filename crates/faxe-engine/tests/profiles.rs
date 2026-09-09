use faxe_engine::{FaxMode, SipProfile, Store, Uuid};

#[test]
fn sending_modes_are_independent_and_legacy_profiles_load_as_auto()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let mut first: SipProfile = serde_json::from_value(serde_json::json!({
        "id": Uuid::new_v4(),
        "name": "First",
        "server": "127.0.0.1",
        "port": 5060,
        "transport": "Udp",
        "username": "fixture",
        "register": false,
        "station_id": "FIXTURE"
    }))?;
    assert_eq!(first.sending_mode, FaxMode::Auto);
    assert_eq!(first.audio_playout_delay_ms, 200);
    first.audio_playout_delay_ms = 360;
    first.sending_mode = FaxMode::G711;
    let mut second = first.clone();
    second.id = Uuid::new_v4();
    second.name = "Second".into();
    second.sending_mode = FaxMode::T38;
    second.audio_playout_delay_ms = 80;
    {
        let mut store = Store::open(directory.path())?;
        store.save_profile(&first)?;
        store.save_profile(&second)?;
    }
    {
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.profile(first.id)?.sending_mode, FaxMode::G711);
        assert_eq!(store.profile(first.id)?.audio_playout_delay_ms, 360);
        assert_eq!(store.profile(second.id)?.sending_mode, FaxMode::T38);
        assert_eq!(store.profile(second.id)?.audio_playout_delay_ms, 80);
        first.sending_mode = FaxMode::Auto;
        store.save_profile(&first)?;
    }
    let store = Store::open(directory.path())?;
    assert_eq!(store.profile(first.id)?.sending_mode, FaxMode::Auto);
    assert_eq!(store.profile(second.id)?.sending_mode, FaxMode::T38);
    for delay in [0, 39, 1001, u16::MAX] {
        first.audio_playout_delay_ms = delay;
        assert!(first.validate().is_err());
    }
    for delay in [40, 200, 1000] {
        first.audio_playout_delay_ms = delay;
        first.validate()?;
    }
    Ok(())
}
