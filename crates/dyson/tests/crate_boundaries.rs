//! Compile-time compatibility checks for focused workspace crate façades.

#[test]
fn core_message_facade_has_one_canonical_type() {
    let through_app = dyson::message::Message::user("hello");
    let through_core: dyson_core::Message = through_app;
    assert_eq!(through_core.last_text(), Some("hello"));
}

#[test]
fn harness_contracts_are_reexported_without_translation() {
    let through_app = dyson::tool::ToolExecutionPlan::read("file:/tmp/input");
    let canonical: dyson_harness::ToolExecutionPlan = through_app;
    assert_eq!(canonical.resources[0].key, "file:/tmp/input");

    let run_id = dyson::agent::protocol::RunId::new();
    let canonical_id: dyson_harness::RunId = run_id;
    assert!(canonical_id.0.starts_with("run-"));
}

#[test]
fn focused_service_types_flow_through_the_application_facades() {
    let ast_config = dyson::ast::config_for_language_name("rust").unwrap();
    let canonical_ast: &dyson_ast::LanguageConfig = ast_config;
    assert_eq!(canonical_ast.display_name, "Rust");

    let severity = dyson::dependency_analysis::Severity::High;
    let canonical_severity: dyson_dependency_analysis::Severity = severity;
    assert_eq!(
        canonical_severity,
        dyson_dependency_analysis::Severity::High
    );
}

#[test]
fn persistence_backend_implements_the_canonical_trait() {
    fn assert_backend<T: dyson_persistence::ChatHistory>() {}
    assert_backend::<dyson::chat_history::DiskChatHistory>();
}
