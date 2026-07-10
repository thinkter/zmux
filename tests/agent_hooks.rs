use zmux::{
    AdapterSettings, AgentAdapter, AgentHookRouter, ControlCommand, HookDelivery, HookFilter,
    NativeResumeCommand, NotificationLevel, NotificationSource, TrustedResumeRecord,
};

/// The first supported vendor adapter is intentionally exercised from its
/// external wire frame all the way to a typed control request and a safe native
/// argv resume request. No listener, shell, or real Codex process is involved.
#[test]
fn codex_hook_routes_to_its_origin_and_resumes_without_shell_interpolation() {
    let frame = br#"{"version":1,"origin":{"workspace_id":41,"surface_id":9},"kind":"permission_request","title":"Needs approval","body":"Allow publishing the preview?","public_session_id":"7f78d154-7513-4b80-b043-6a8f6b969d16"}"#;
    let settings = AdapterSettings {
        enabled: true,
        resume_enabled: true,
    };

    let adapter_event = AgentAdapter::Codex
        .parse_opt_in_hook(settings, frame)
        .expect("explicitly enabled adapter accepts its documented frame");
    assert_eq!(adapter_event.event().agent, "codex");

    let mut router = AgentHookRouter::new(HookFilter {
        show_subagents: false,
        show_teammates: false,
    });
    let routed = router
        .route(adapter_event.clone().into_event())
        .expect("validated adapter event routes");
    assert_eq!(routed.delivery, HookDelivery::Delivered);
    assert_eq!(
        routed.into_control_request(501).unwrap().command,
        ControlCommand::NotificationCreate {
            workspace_id: 41,
            surface_id: 9,
            source: NotificationSource::AgentHook,
            level: NotificationLevel::Warning,
            title: "codex: Needs approval".to_string(),
            body: "Allow publishing the preview?".to_string(),
        }
    );

    let record = TrustedResumeRecord::from_adapter_event(settings, &adapter_event)
        .expect("enabled Codex adapter supplies a safe public ID");
    let encoded = record.encode().unwrap();
    let persisted = String::from_utf8(encoded.clone()).unwrap();
    assert!(persisted.contains("public_session_id"));
    assert!(!persisted.contains("workspace_id"));
    assert!(!persisted.contains("surface_id"));
    assert!(!persisted.contains("Allow publishing"));

    let loaded = TrustedResumeRecord::decode(&encoded).unwrap();
    assert_eq!(
        loaded.native_command().unwrap(),
        NativeResumeCommand {
            program: "codex",
            args: vec![
                "resume".to_string(),
                "7f78d154-7513-4b80-b043-6a8f6b969d16".to_string(),
            ],
        }
    );
}
