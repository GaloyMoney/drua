//! Heuristic classifier for architectural role classification.
//!
//! Assigns a single primary label, a layer tag (from file path), and multiple
//! uses tags (from content) to each code chunk. The primary label is chosen
//! by priority (most-specific-first), not confidence.

use serde::{Deserialize, Serialize};

/// A single architectural role label with confidence and provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabeledRole {
    pub role: String,
    pub confidence: f32,
    pub signals: Vec<String>,
}

/// Minimal chunk data needed for labeling (avoids coupling to store types).
#[derive(Debug, Clone)]
pub struct ChunkData {
    pub content: String,
    pub file_path: String,
    pub chunk_type: String,
    pub entity_name: String,
}

/// Three-facet classification result for a code chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkClassification {
    /// The single primary architectural label (if any).
    pub primary_label: Option<String>,
    /// Confidence of the primary label (0.0–1.0).
    pub primary_confidence: f32,
    /// Signals that led to the primary label.
    pub primary_signals: Vec<String>,
    /// Architectural layer derived from file path.
    pub layer: Option<String>,
    /// Content-derived usage tags.
    pub uses: Vec<String>,
}

/// All primary labels in the taxonomy.
pub const KNOWN_PRIMARY_LABELS: &[&str] = &[
    // Backend (Rust)
    "entity",
    "entity_command",
    "entity_query",
    "entity_hydration",
    "entity_event",
    "published_event",
    "new_entity",
    "service_method",
    "service",
    "repository",
    "error",
    "authorization",
    "value_object",
    "domain_primitives",
    "api",
    "job",
    "event_handler",
    "type_conversion",
    "test",
    "config",
    // Frontend (JS/TS)
    "component",
    "page",
    "layout",
    "hook",
    "graphql_query",
    "graphql_mutation",
    "ui_primitive",
    "store",
    "route_handler",
    "style_variant",
];

/// The 4 layer values.
pub const KNOWN_LAYERS: &[&str] = &["domain", "application", "infrastructure", "interface"];

/// The 5 uses tags.
pub const KNOWN_USES: &[&str] = &[
    "repository",
    "events",
    "authorization",
    "publisher",
    "config",
];

/// Priority order: most specific first. When multiple checkers fire,
/// the first match in this list wins.
const PRIORITY_ORDER: &[&str] = &[
    // Backend
    "entity_hydration",
    "entity_command",
    "entity_query",
    "new_entity",
    "entity_event",
    "published_event",
    "entity",
    "event_handler",
    "job",
    "type_conversion",
    "authorization",
    // Frontend labels that should beat generic "api"
    "hook",
    "graphql_mutation",
    "graphql_query",
    "page",
    "layout",
    "route_handler",
    // Back to backend
    "api",
    "service_method",
    "service",
    "repository",
    "error",
    "config",
    "test",
    "domain_primitives",
    "value_object",
    // Remaining frontend
    "store",
    "style_variant",
    "ui_primitive",
    "component",
];

/// All primary-label checker functions.
const PRIMARY_CHECKERS: &[fn(&ChunkData) -> Option<LabeledRole>] = &[
    // Backend
    check_entity,
    check_entity_event,
    check_error,
    check_repository,
    check_service,
    check_entity_command,
    check_entity_query,
    check_service_method,
    check_authorization,
    check_value_object,
    check_domain_primitives,
    check_new_entity,
    check_entity_hydration,
    check_test,
    check_job,
    check_event_handler,
    check_published_event,
    check_api_handler,
    check_config,
    check_type_conversion,
    // Frontend
    check_component,
    check_page,
    check_layout,
    check_hook,
    check_graphql_query,
    check_graphql_mutation,
    check_ui_primitive,
    check_store,
    check_route_handler,
    check_style_variant,
];

/// Standard trait impls that are always boilerplate — never labeled.
const BOILERPLATE_TRAITS: &[&str] = &[
    "Clone",
    "Debug",
    "Display",
    "Default",
    "PartialEq",
    "Eq",
    "Hash",
    "PartialOrd",
    "Ord",
];

/// Classify a chunk into a single primary label + layer + uses.
pub fn classify_chunk(chunk: &ChunkData) -> ChunkClassification {
    // 0. Skip impl_summary and boilerplate trait impls — no architectural role.
    if chunk.chunk_type == "impl_summary"
        || (is_trait_impl(&chunk.content) && is_boilerplate_trait_impl(&chunk.content))
    {
        return ChunkClassification {
            primary_label: None,
            primary_confidence: 0.0,
            primary_signals: vec![],
            layer: detect_layer(chunk),
            uses: vec![],
        };
    }

    // 1. Run all checkers, collect candidates
    let candidates: Vec<LabeledRole> = PRIMARY_CHECKERS
        .iter()
        .filter_map(|check| check(chunk))
        .collect();

    // 2. Pick winner by priority order (most-specific-first)
    let winner = PRIORITY_ORDER
        .iter()
        .find_map(|&label| candidates.iter().find(|c| c.role == label).cloned());

    // 3. Layer from file path (needed before fallback)
    let layer = detect_layer(chunk);

    // 4. Fallback: unmatched struct_item in domain layer → value_object at 0.5
    let winner = winner.or_else(|| {
        if chunk.chunk_type == "struct_item"
            && candidates.is_empty()
            && layer.as_deref() == Some("domain")
        {
            Some(LabeledRole {
                role: "value_object".to_string(),
                confidence: 0.5,
                signals: vec!["fallback:struct_in_domain_layer".to_string()],
            })
        } else {
            None
        }
    });

    // 5. Uses from content
    let uses = detect_uses(chunk);

    match winner {
        Some(w) => ChunkClassification {
            primary_label: Some(w.role),
            primary_confidence: w.confidence,
            primary_signals: w.signals,
            layer,
            uses,
        },
        None => ChunkClassification {
            primary_label: None,
            primary_confidence: 0.0,
            primary_signals: vec![],
            layer,
            uses,
        },
    }
}

/// Backward-compat shim: returns 0–1 roles from classify_chunk().
pub fn label_chunk(chunk: &ChunkData) -> Vec<LabeledRole> {
    let cls = classify_chunk(chunk);
    match cls.primary_label {
        Some(label) => vec![LabeledRole {
            role: label,
            confidence: cls.primary_confidence,
            signals: cls.primary_signals,
        }],
        None => vec![],
    }
}

/// Detect architectural layer from file path.
fn detect_layer(c: &ChunkData) -> Option<String> {
    // Tests get no layer
    if c.file_path.contains("tests/")
        || c.content.contains("#[test]")
        || c.content.contains("#[tokio::test]")
        || c.chunk_type == "test_block"
    {
        return None;
    }
    // JS/TS test files
    if is_js_ts_file(&c.file_path)
        && (c.file_path.contains(".test.")
            || c.file_path.contains(".spec.")
            || c.file_path.contains("/__tests__/"))
    {
        return None;
    }

    // Interface layer
    if c.file_path.contains("graphql/") || c.file_path.contains("server/") {
        return Some("interface".to_string());
    }
    // Infrastructure layer
    if c.file_path.ends_with("repo.rs")
        || c.file_path.contains("publisher.rs")
        || c.file_path.contains("jobs/")
        || c.file_path.ends_with("job.rs")
        || c.file_path.contains("client.rs")
        || c.file_path.contains("gateway.rs")
    {
        return Some("infrastructure".to_string());
    }
    // JS/TS infrastructure: API routes, store/state management (check before interface)
    if is_js_ts_file(&c.file_path)
        && (c.file_path.contains("/api/")
            || c.file_path.contains("/store/")
            || c.file_path.contains("/stores/"))
    {
        return Some("infrastructure".to_string());
    }

    // JS/TS interface layer (pages, components, layouts)
    if is_js_ts_file(&c.file_path)
        && (c.file_path.contains("/pages/")
            || c.file_path.contains("/app/")
            || c.file_path.contains("/components/")
            || c.file_path.contains("/layouts/"))
    {
        return Some("interface".to_string());
    }

    // Application layer
    if c.file_path.ends_with("mod.rs") || c.file_path.ends_with("lib.rs") {
        return Some("application".to_string());
    }
    // JS/TS application: hooks (business logic)
    if is_js_ts_file(&c.file_path) && c.file_path.contains("/hooks/") {
        return Some("application".to_string());
    }

    // Domain layer
    if c.file_path.ends_with("entity.rs")
        || c.file_path.contains("primitives.rs")
        || c.file_path.ends_with("error.rs")
    {
        return Some("domain".to_string());
    }

    None
}

/// Detect content-based usage tags.
fn detect_uses(c: &ChunkData) -> Vec<String> {
    let mut uses = Vec::new();

    if c.content.contains("self.repo.")
        || c.content.contains(".find_by_id(")
        || c.content.contains(".begin_op(")
    {
        uses.push("repository".to_string());
    }
    if c.content.contains("self.events.push(") {
        uses.push("events".to_string());
    }
    if c.content.contains("enforce_permission") || c.content.contains("self.authz") {
        uses.push("authorization".to_string());
    }
    if c.content.contains("publish_all_persisted") || c.content.contains("publish_in_op") {
        uses.push("publisher".to_string());
    }
    if c.content.contains("ExposedDomainConfig")
        || c.content.contains("DomainConfig")
        || c.content.contains("define_exposed_config")
    {
        uses.push("config".to_string());
    }

    uses
}

// ---------- primary label checkers ----------

fn check_entity(c: &ChunkData) -> Option<LabeledRole> {
    if is_in_macro_crate(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    let has_entity_events = c.chunk_type == "struct_item" && c.content.contains("EntityEvents<");
    let has_es_entity_derive = has_derive(&c.content, "EsEntity");

    if has_entity_events {
        signals.push("field_type:EntityEvents".to_string());
    }
    if has_es_entity_derive {
        signals.push("derive:EsEntity".to_string());
    }
    if c.file_path.contains("/entity.rs")
        && c.chunk_type == "struct_item"
        && (has_entity_events || has_es_entity_derive)
    {
        signals.push("file_path:/entity.rs+struct_item".to_string());
    }

    some_if("entity", 0.95, signals)
}

/// Entity event — domain event enum (renamed from check_domain_event).
/// Also absorbs private_event (non-public entity events).
fn check_entity_event(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.file_path.contains("/entity.rs")
        && c.chunk_type == "enum_item"
        && c.content.contains("serde(tag")
    {
        signals.push("file_path:/entity.rs+enum+serde_tag".to_string());
    }
    if has_derive(&c.content, "EsEvent") {
        signals.push("derive:EsEvent".to_string());
    }

    some_if("entity_event", 0.95, signals)
}

fn check_error(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.file_path.ends_with("error.rs") {
        signals.push("file_path:error.rs".to_string());
    }
    if c.entity_name.ends_with("Error") && c.chunk_type == "enum_item" {
        signals.push("entity_name:*Error+enum".to_string());
    }
    if has_derive(&c.content, "Error") && c.content.contains("thiserror") {
        signals.push("derive:thiserror::Error".to_string());
    }

    some_if("error", 0.95, signals)
}

fn check_repository(c: &ChunkData) -> Option<LabeledRole> {
    if is_trait_impl(&c.content) {
        return None;
    }
    let mut signals = Vec::new();

    if c.file_path.ends_with("repo.rs") {
        signals.push("file_path:repo.rs".to_string());
    }
    if has_derive(&c.content, "EsRepo") {
        signals.push("derive:EsRepo".to_string());
    }
    if c.entity_name.ends_with("Repo") {
        signals.push("entity_name:*Repo".to_string());
    }

    some_if("repository", 0.95, signals)
}

/// Service — struct that acts as a service. Merges old domain_service,
/// application_service, external_service into one label.
fn check_service(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    let in_mod_or_lib = c.file_path.ends_with("mod.rs") || c.file_path.ends_with("lib.rs");
    if in_mod_or_lib && c.chunk_type == "struct_item" && has_repo_field(&c.content) {
        signals.push("mod/lib+struct_with_repo_field".to_string());
    }
    if c.entity_name.ends_with("Service") {
        signals.push("entity_name:*Service".to_string());
    }
    if is_plural_noun(&c.entity_name) && c.chunk_type == "struct_item" {
        signals.push("entity_name:plural_noun".to_string());
    }
    // Application service pattern: multiple Arc service deps
    if c.chunk_type == "struct_item" && in_mod_or_lib && has_arc_service_fields(&c.content) {
        signals.push("struct:multiple_arc_service_fields".to_string());
    }
    // External service pattern: wraps HTTP client
    if c.chunk_type == "struct_item"
        && has_client_field(&c.content)
        && !c.file_path.contains("jobs/")
        && !c.file_path.contains("job.rs")
    {
        signals.push("struct:client_field".to_string());
    }
    if c.file_path.contains("client.rs")
        || c.file_path.contains("gateway.rs")
        || c.file_path.contains("adapter.rs")
    {
        signals.push("file_path:external_integration".to_string());
    }
    // Service init/constructor methods in service-like files
    let in_service_file = in_mod_or_lib
        || c.file_path.contains("client.rs")
        || c.file_path.contains("gateway.rs")
        || c.file_path.contains("adapter.rs");
    if in_service_file
        && is_method_chunk(&c.chunk_type)
        && is_service_init(&c.entity_name, &c.content)
    {
        signals.push("service_file+service_init".to_string());
    }

    some_if("service", 0.85, signals)
}

/// Entity command — mutation method on an entity (`&mut self`).
fn check_entity_command(c: &ChunkData) -> Option<LabeledRole> {
    if c.chunk_type == "impl_summary" {
        return None;
    }
    if is_trait_impl(&c.content) {
        return None;
    }
    let mut signals = Vec::new();

    if c.content.contains("idempotency_guard!") {
        signals.push("macro:idempotency_guard".to_string());
    }
    if c.content.contains("Idempotent<") {
        signals.push("return_type:Idempotent".to_string());
    }
    if c.content.contains("self.events.push(") {
        signals.push("call:self.events.push".to_string());
    }
    if is_method_chunk(&c.chunk_type)
        && c.content.contains("&mut self")
        && c.file_path.contains("entity.rs")
        && !c.entity_name.contains("Builder")
        && !is_builder_pattern(&c.content)
    {
        signals.push("method:&mut_self+entity.rs".to_string());
    }

    if signals.is_empty() {
        return None;
    }

    let confidence = if signals
        .iter()
        .any(|s| s.starts_with("macro:") || s.starts_with("return_type:") || s.starts_with("call:"))
    {
        0.95
    } else {
        0.7
    };

    Some(LabeledRole {
        role: "entity_command".to_string(),
        confidence,
        signals,
    })
}

/// Entity query — read method on an entity (`&self`, returns value).
fn check_entity_query(c: &ChunkData) -> Option<LabeledRole> {
    if !is_method_chunk(&c.chunk_type) {
        return None;
    }
    if is_trait_impl(&c.content) {
        return None;
    }
    if !c.content.contains("&self") || c.content.contains("&mut self") {
        return None;
    }
    if !c.content.contains("->") {
        return None;
    }
    if !c.file_path.contains("entity.rs") {
        return None;
    }

    Some(LabeledRole {
        role: "entity_query".to_string(),
        confidence: 0.6,
        signals: vec!["method:&self+returns_value+entity.rs".to_string()],
    })
}

/// Service method — public method on a service struct (renamed from use_case).
fn check_service_method(c: &ChunkData) -> Option<LabeledRole> {
    if !is_method_chunk(&c.chunk_type) {
        return None;
    }

    // begin_op is a boilerplate helper, not a real service method
    if c.entity_name == "begin_op" {
        return None;
    }

    let mut signals = Vec::new();

    if c.entity_name.ends_with("_in_op") {
        signals.push("method_name:*_in_op".to_string());
    }
    if c.content.contains("DbOp<") {
        signals.push("param:DbOp".to_string());
    }
    if c.content.contains("begin_op()") {
        signals.push("call:begin_op".to_string());
    }
    let in_mod_or_lib = c.file_path.ends_with("mod.rs") || c.file_path.ends_with("lib.rs");
    if in_mod_or_lib
        && c.content.contains("pub async fn")
        && !is_service_init(&c.entity_name, &c.content)
    {
        signals.push("mod/lib+pub_async_fn".to_string());
    }

    some_if("service_method", 0.8, signals)
}

fn check_authorization(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.content.contains("evaluate_permission") {
        signals.push("call:evaluate_permission".to_string());
    }
    // self.authz calling actual authz methods (not clone/new/field access)
    if (c.content.contains("self.authz.evaluate")
        || c.content.contains("self.authz.audit")
        || c.content.contains("self.authz.enforce"))
        && c.chunk_type != "impl_summary"
    {
        signals.push("call:self.authz".to_string());
    }
    if c.file_path.contains("authz") || c.file_path.contains("authorization") {
        signals.push("file_path:authz".to_string());
    }

    some_if("authorization", 0.85, signals)
}

fn check_value_object(c: &ChunkData) -> Option<LabeledRole> {
    if c.chunk_type != "struct_item" {
        return None;
    }
    let mut signals = Vec::new();

    if c.file_path.contains("primitives.rs") && is_newtype_struct(&c.content) {
        signals.push("file_path:primitives.rs+newtype".to_string());
    }

    some_if("value_object", 0.85, signals)
}

/// Domain primitives — types in primitives.rs, including entity_id! macro invocations.
fn check_domain_primitives(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.file_path.contains("primitives.rs") {
        signals.push("file_path:primitives.rs".to_string());
    }
    if c.content.contains("entity_id!") {
        signals.push("macro:entity_id!".to_string());
    }
    if c.entity_name.ends_with("Id") && c.entity_name.len() > 2 {
        signals.push("entity_name:*Id".to_string());
    }

    some_if("domain_primitives", 0.90, signals)
}

fn check_new_entity(c: &ChunkData) -> Option<LabeledRole> {
    if !c.entity_name.starts_with("New") {
        return None;
    }
    let mut signals = Vec::new();

    // Struct definition with Builder derive
    if c.chunk_type == "struct_item" {
        if has_derive(&c.content, "Builder") || c.content.contains("derive_builder") {
            signals.push("entity_name:New*+builder".to_string());
        }
        // Struct named New* in entity.rs is likely a new-entity even without Builder
        if c.file_path.contains("entity.rs") {
            signals.push("entity_name:New*+entity.rs".to_string());
        }
    }
    // IntoEvents impl for a New* type
    if c.content.contains("IntoEvents") {
        signals.push("entity_name:New*+IntoEvents".to_string());
    }
    // Impl block for a New* type (e.g. NewAccountBuilder methods)
    if (c.chunk_type == "impl_item" || c.chunk_type == "impl_method") && !is_trait_impl(&c.content)
    {
        signals.push("entity_name:New*+impl".to_string());
    }

    some_if("new_entity", 0.95, signals)
}

/// Entity hydration — TryFromEvents impl (renamed from check_hydration).
fn check_entity_hydration(c: &ChunkData) -> Option<LabeledRole> {
    if (c.chunk_type == "impl_item" || c.chunk_type == "impl_method")
        && c.content.contains("TryFromEvents")
    {
        Some(LabeledRole {
            role: "entity_hydration".to_string(),
            confidence: 0.95,
            signals: vec!["impl_trait:TryFromEvents".to_string()],
        })
    } else {
        None
    }
}

fn check_test(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.chunk_type == "test_block" {
        signals.push("chunk_type:test_block".to_string());
    }
    if c.file_path.contains("_test.rs") || c.file_path.contains("tests/") {
        signals.push("file_path:test".to_string());
    }
    if c.content.contains("#[test]") || c.content.contains("#[tokio::test]") {
        signals.push("attr:test".to_string());
    }
    if c.content.contains("#[cfg(test)]") {
        signals.push("attr:cfg_test".to_string());
    }
    // JS/TS test patterns
    if is_js_ts_file(&c.file_path) {
        if c.file_path.contains(".test.") || c.file_path.contains(".spec.") {
            signals.push("file_path:test/spec".to_string());
        }
        if c.file_path.contains("/__tests__/") {
            signals.push("file_path:__tests__/".to_string());
        }
        if (c.content.contains("describe(")
            || c.content.contains("it(")
            || c.content.contains("test("))
            && (c.content.contains("expect(") || c.content.contains("assert"))
        {
            signals.push("content:jest/vitest_pattern".to_string());
        }
    }

    some_if("test", 0.95, signals)
}

fn check_job(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.content.contains("JobInitializer") {
        signals.push("impl:JobInitializer".to_string());
    }
    if c.content.contains("JobRunner") {
        signals.push("impl:JobRunner".to_string());
    }
    if c.content.contains("CurrentJob") {
        signals.push("type:CurrentJob".to_string());
    }
    if c.content.contains("JobCompletion") {
        signals.push("type:JobCompletion".to_string());
    }
    if c.file_path.ends_with("job.rs") || c.file_path.contains("/jobs/") {
        signals.push("file_path:job".to_string());
    }

    some_if("job", 0.90, signals)
}

fn check_event_handler(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.content.contains("OutboxEventHandler") {
        signals.push("impl:OutboxEventHandler".to_string());
    }
    if c.content.contains("handle_persistent") {
        signals.push("method:handle_persistent".to_string());
    }
    if c.content.contains("PersistentOutboxEvent") {
        signals.push("type:PersistentOutboxEvent".to_string());
    }

    some_if("event_handler", 0.90, signals)
}

fn check_published_event(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    // Published event enum in public/event.rs or public/mod.rs
    if (c.file_path.contains("public/event.rs") || c.file_path.contains("public/mod.rs"))
        && c.chunk_type == "enum_item"
    {
        signals.push("file_path:public/event+enum".to_string());
    }
    // Explicit impl OutboxEventMarker (e.g. macro-generated or hand-written)
    if c.content.contains("impl OutboxEventMarker") {
        signals.push("trait:impl_OutboxEventMarker".to_string());
    }
    // Publisher struct in publisher.rs (e.g. DepositPublisher<E>)
    if c.file_path.contains("publisher.rs") && c.chunk_type == "struct_item" {
        signals.push("file_path:publisher.rs+struct".to_string());
    }
    // Publisher method in publisher.rs (e.g. publish_deposit_in_op)
    if c.file_path.contains("publisher.rs")
        && is_method_chunk(&c.chunk_type)
        && !is_service_init(&c.entity_name, &c.content)
    {
        signals.push("file_path:publisher.rs+method".to_string());
    }
    // Struct or impl with OutboxEventMarker<T> bound
    if c.content.contains("OutboxEventMarker<")
        && (c.chunk_type == "struct_item"
            || c.chunk_type == "impl_item"
            || c.chunk_type == "impl_method")
    {
        signals.push("bound:OutboxEventMarker".to_string());
    }
    // Struct with Outbox<E> field (publisher infrastructure)
    if c.chunk_type == "struct_item" && has_outbox_field(&c.content) {
        signals.push("field_type:Outbox".to_string());
    }
    some_if("published_event", 0.90, signals)
}

fn check_api_handler(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if has_derive(&c.content, "Object")
        || has_derive(&c.content, "SimpleObject")
        || has_derive(&c.content, "InputObject")
    {
        signals.push("derive:async_graphql_object".to_string());
    }
    if c.content.contains("#[Object]") || c.content.contains("#[async_graphql::Object]") {
        signals.push("attr:graphql_object".to_string());
    }
    if c.file_path.contains("graphql/") || c.file_path.contains("graphql.rs") {
        signals.push("file_path:graphql".to_string());
    }
    if c.file_path.contains("server/") && c.content.contains("axum::") {
        signals.push("file_path:server+axum".to_string());
    }
    if c.content.contains("MutationRoot") || c.content.contains("QueryRoot") {
        signals.push("type:graphql_root".to_string());
    }

    some_if("api", 0.85, signals)
}

fn check_config(c: &ChunkData) -> Option<LabeledRole> {
    let mut signals = Vec::new();

    if c.file_path.contains("config.rs") || c.file_path.contains("config/") {
        signals.push("file_path:config".to_string());
    }
    if c.entity_name.ends_with("Config") || c.entity_name.ends_with("Settings") {
        signals.push("entity_name:*Config/*Settings".to_string());
    }
    if has_derive(&c.content, "Deserialize") && c.content.contains("default") {
        signals.push("derive:Deserialize+default".to_string());
    }

    some_if("config", 0.85, signals)
}

fn check_type_conversion(c: &ChunkData) -> Option<LabeledRole> {
    if c.chunk_type == "impl_summary" {
        return None;
    }
    let mut signals = Vec::new();

    if (c.chunk_type == "impl_item" || c.chunk_type == "impl_method")
        && (c.content.contains("impl From<") || c.content.contains("impl TryFrom<"))
    {
        signals.push("impl:From/TryFrom".to_string());
    }
    if c.chunk_type == "impl_item" && c.content.contains("impl Into<") {
        signals.push("impl:Into".to_string());
    }
    if c.file_path.contains("convert") || c.file_path.contains("mapping") {
        signals.push("file_path:convert/mapping".to_string());
    }

    some_if("type_conversion", 0.80, signals)
}

// ---------- frontend (JS/TS) checkers ----------

/// React/Vue/Svelte component — function or arrow returning JSX, or in components/ dir.
fn check_component(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    // File in a components/ directory
    if c.file_path.contains("/components/") || c.file_path.contains("/Components/") {
        signals.push("file_path:components/".to_string());
    }

    // PascalCase function/arrow that returns JSX
    let is_pascal = c
        .entity_name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_uppercase());
    let is_fn = c.chunk_type == "function_declaration" || c.chunk_type == "arrow_function";
    if is_fn && is_pascal && has_jsx_return(&c.content) {
        signals.push("pascal_case_fn+jsx_return".to_string());
    }

    // forwardRef pattern
    if c.content.contains("forwardRef") {
        signals.push("call:forwardRef".to_string());
    }

    some_if("component", 0.85, signals)
}

/// Page component — in pages/ or app/ directory.
fn check_page(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    if c.file_path.contains("/pages/") || c.file_path.contains("/Pages/") {
        signals.push("file_path:pages/".to_string());
    }
    // Next.js app directory page files
    if c.file_path.contains("/app/")
        && path_ends_with_any(
            &c.file_path,
            &["page.tsx", "page.jsx", "page.ts", "page.js"],
        )
    {
        signals.push("file_path:app/page".to_string());
    }
    // Default export in pages dir (common Next.js pattern)
    if signals.is_empty()
        && c.chunk_type == "export_statement"
        && c.content.contains("default")
        && c.file_path.contains("/pages/")
    {
        signals.push("file_path:pages/+default_export".to_string());
    }

    some_if("page", 0.90, signals)
}

/// Layout component — in layouts/ or named layout.
fn check_layout(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    if c.file_path.contains("/layouts/") || c.file_path.contains("/Layouts/") {
        signals.push("file_path:layouts/".to_string());
    }
    if path_ends_with_any(
        &c.file_path,
        &["layout.tsx", "layout.jsx", "layout.ts", "layout.js"],
    ) {
        signals.push("file_path:layout.*".to_string());
    }
    let name_lower = c.entity_name.to_lowercase();
    if name_lower.ends_with("layout") || name_lower.starts_with("layout") {
        signals.push("entity_name:*layout*".to_string());
    }

    some_if("layout", 0.85, signals)
}

/// React hook — function/arrow starting with `use`.
fn check_hook(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    let is_fn = c.chunk_type == "function_declaration" || c.chunk_type == "arrow_function";
    if is_fn && c.entity_name.starts_with("use") && c.entity_name.len() > 3 {
        // Ensure the character after "use" is uppercase (useAuth, useQuery, etc.)
        let after_use = &c.entity_name[3..];
        if after_use.starts_with(|ch: char| ch.is_uppercase()) {
            signals.push("entity_name:use*".to_string());
        }
    }

    if c.file_path.contains("/hooks/") || c.file_path.contains("/Hooks/") {
        signals.push("file_path:hooks/".to_string());
    }

    some_if("hook", 0.95, signals)
}

/// GraphQL query — gql tagged template or .graphql file with query.
fn check_graphql_query(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    let has_gql_tag = c.content.contains("gql`") || c.content.contains("graphql`");
    let is_query = c.content.contains("query ") || c.content.contains("query(");

    if has_gql_tag && is_query && !c.content.contains("mutation ") {
        signals.push("gql_tagged_template+query".to_string());
    }
    if c.content.contains("useQuery(")
        || c.content.contains("useLazyQuery(")
        || c.content.contains("useSuspenseQuery(")
    {
        signals.push("call:useQuery".to_string());
    }
    if has_gql_tag
        && (c.entity_name.starts_with("GET_")
            || c.entity_name.starts_with("FETCH_")
            || c.entity_name.starts_with("LIST_")
            || c.entity_name.ends_with("_QUERY")
            || c.entity_name.ends_with("Query"))
    {
        signals.push("entity_name:query_pattern+gql".to_string());
    }

    some_if("graphql_query", 0.90, signals)
}

/// GraphQL mutation — gql tagged template with mutation keyword.
fn check_graphql_mutation(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    let has_gql_tag = c.content.contains("gql`") || c.content.contains("graphql`");

    if has_gql_tag && c.content.contains("mutation ") {
        signals.push("gql_tagged_template+mutation".to_string());
    }
    if c.content.contains("useMutation(") {
        signals.push("call:useMutation".to_string());
    }
    if (c.entity_name.starts_with("CREATE_")
        || c.entity_name.starts_with("UPDATE_")
        || c.entity_name.starts_with("DELETE_")
        || c.entity_name.ends_with("_MUTATION")
        || c.entity_name.ends_with("Mutation"))
        && has_gql_tag
    {
        signals.push("entity_name:mutation_pattern+gql".to_string());
    }

    some_if("graphql_mutation", 0.90, signals)
}

/// UI primitive — small reusable component in a ui/ or primitives/ directory.
fn check_ui_primitive(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    // Require the file to be in a ui/ or primitives/ directory
    if !c.file_path.contains("/ui/") && !c.file_path.contains("/primitives/") {
        return None;
    }
    let mut signals = Vec::new();
    signals.push("file_path:ui/|primitives/".to_string());

    // Boost confidence if name matches common UI primitive names
    let name_lower = c.entity_name.to_lowercase();
    let ui_names = [
        "button",
        "input",
        "select",
        "checkbox",
        "radio",
        "toggle",
        "switch",
        "modal",
        "dialog",
        "tooltip",
        "popover",
        "dropdown",
        "badge",
        "avatar",
        "spinner",
        "skeleton",
        "divider",
        "separator",
        "icon",
        "label",
        "textarea",
    ];
    if ui_names.iter().any(|n| name_lower == *n) {
        signals.push("entity_name:ui_primitive".to_string());
    }

    some_if("ui_primitive", 0.80, signals)
}

/// State store — Redux, Zustand, MobX, or similar state management.
fn check_store(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    if c.file_path.contains("/store/") || c.file_path.contains("/stores/") {
        signals.push("file_path:store/".to_string());
    }
    if c.content.contains("createSlice(") || c.content.contains("createStore(") {
        signals.push("call:createSlice/createStore".to_string());
    }
    if c.content.contains("zustand") || c.content.contains("create(") && c.content.contains("set(")
    {
        signals.push("pattern:zustand".to_string());
    }
    if c.content.contains("configureStore(") || c.content.contains("combineReducers(") {
        signals.push("call:redux_configure".to_string());
    }
    if c.entity_name.ends_with("Store")
        || c.entity_name.ends_with("Slice")
        || c.entity_name.ends_with("Reducer")
    {
        signals.push("entity_name:*Store/*Slice/*Reducer".to_string());
    }

    some_if("store", 0.85, signals)
}

/// Route handler — Express/Next.js API route handler.
fn check_route_handler(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    // Next.js API routes
    if c.file_path.contains("/api/")
        && path_ends_with_any(
            &c.file_path,
            &["route.ts", "route.js", "route.tsx", "route.jsx"],
        )
    {
        signals.push("file_path:api/route".to_string());
    }
    // Express-style route handlers
    if c.content.contains("app.get(")
        || c.content.contains("app.post(")
        || c.content.contains("app.put(")
        || c.content.contains("app.delete(")
        || c.content.contains("router.get(")
        || c.content.contains("router.post(")
        || c.content.contains("router.put(")
        || c.content.contains("router.delete(")
    {
        signals.push("call:express_route".to_string());
    }
    // Next.js API route exports
    if c.file_path.contains("/api/")
        && (c.entity_name == "GET"
            || c.entity_name == "POST"
            || c.entity_name == "PUT"
            || c.entity_name == "DELETE"
            || c.entity_name == "PATCH")
    {
        signals.push("export:http_method_handler".to_string());
    }

    some_if("route_handler", 0.85, signals)
}

/// Style variant — styled-components, CSS-in-JS, or cva/class-variance-authority.
fn check_style_variant(c: &ChunkData) -> Option<LabeledRole> {
    if !is_js_ts_file(&c.file_path) {
        return None;
    }
    let mut signals = Vec::new();

    if c.content.contains("styled.") || c.content.contains("styled(") {
        signals.push("call:styled".to_string());
    }
    if c.content.contains("cva(") || c.content.contains("class-variance-authority") {
        signals.push("call:cva".to_string());
    }
    if c.content.contains("css`") || c.content.contains("css({") {
        signals.push("css_in_js".to_string());
    }
    let name_lower = c.entity_name.to_lowercase();
    if name_lower.ends_with("variants") || name_lower.ends_with("styles") {
        signals.push("entity_name:*variants/*styles".to_string());
    }

    some_if("style_variant", 0.80, signals)
}

// ---------- frontend helpers ----------

/// Check if the file path is a JS/TS file.
fn is_js_ts_file(path: &str) -> bool {
    path.ends_with(".js")
        || path.ends_with(".jsx")
        || path.ends_with(".ts")
        || path.ends_with(".tsx")
        || path.ends_with(".mjs")
        || path.ends_with(".cjs")
}

/// Check if content contains JSX return patterns.
fn has_jsx_return(content: &str) -> bool {
    // JSX return: `return (<div`, `return <div`, or template with < in return
    content.contains("return (") && content.contains('<')
        || content.contains("return <")
        || content.contains("=> (") && content.contains('<')
        || content.contains("=> <")
}

/// Check if a file path ends with any of the given suffixes.
fn path_ends_with_any(path: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|s| path.ends_with(s))
}

// ---------- helpers ----------

fn is_builder_pattern(content: &str) -> bool {
    content.contains("&mut self")
        && (content.contains("-> &mut Self") || content.contains("-> Result<&mut Self"))
}

fn is_trait_impl(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim().trim_start_matches("// ");
        if let Some(rest) = trimmed.strip_prefix("impl ") {
            if rest.contains(" for ") {
                return true;
            }
        }
    }
    false
}

fn is_in_macro_crate(file_path: &str) -> bool {
    file_path.contains("-macros/") || file_path.contains("_macros/")
}

fn some_if(role: &str, confidence: f32, signals: Vec<String>) -> Option<LabeledRole> {
    if signals.is_empty() {
        return None;
    }
    Some(LabeledRole {
        role: role.to_string(),
        confidence,
        signals,
    })
}

fn has_derive(content: &str, name: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("#[derive(") || trimmed.starts_with("#[derive ("))
            && trimmed.contains(name)
        {
            return true;
        }
        if trimmed.starts_with("#[") && trimmed.contains("derive") && trimmed.contains(name) {
            return true;
        }
    }
    false
}

fn has_repo_field(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("Repo") && trimmed.contains(':') && !trimmed.starts_with("//") {
            return true;
        }
    }
    false
}

fn has_outbox_field(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.contains(':') && trimmed.contains("Outbox<") {
            return true;
        }
    }
    false
}

fn has_client_field(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.contains(':')
            && (trimmed.contains("Client")
                || trimmed.contains("reqwest")
                || trimmed.contains("tonic"))
        {
            return true;
        }
    }
    false
}

fn has_arc_service_fields(content: &str) -> bool {
    let mut arc_count = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.contains("Arc<") && trimmed.contains(':') {
            arc_count += 1;
        }
    }
    arc_count >= 3
}

fn is_plural_noun(name: &str) -> bool {
    const KNOWN_PLURALS: &[&str] = &[
        "Customers",
        "Accounts",
        "Deposits",
        "Withdrawals",
        "Transactions",
        "Ledgers",
        "Journals",
        "Entries",
        "Users",
        "Wallets",
        "Payments",
        "Invoices",
        "CreditFacilities",
        "Approvals",
        "Documents",
        "Reports",
        "Loans",
        "Committees",
        "TermLoans",
        "Obligations",
        "Disbursals",
        "Collaterals",
        "RepaymentPlans",
    ];

    if KNOWN_PLURALS.contains(&name) {
        return true;
    }

    if name.ends_with('s')
        && !name.ends_with("ss")
        && !name.ends_with("us")
        && !name.ends_with("is")
        && !name.ends_with("Values")
        && !name.ends_with("Params")
        && !name.ends_with("Args")
        && !name.ends_with("Options")
        && !name.ends_with("Settings")
        && !name.ends_with("Details")
        && !name.ends_with("Fields")
        && !name.ends_with("Columns")
        && name.len() > 3
        && name.chars().next().is_some_and(|c| c.is_uppercase())
    {
        return true;
    }

    false
}

fn is_method_chunk(chunk_type: &str) -> bool {
    chunk_type == "impl_method" || chunk_type == "impl_item" || chunk_type == "function_item"
}

fn is_newtype_struct(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("pub struct") || trimmed.starts_with("struct"))
            && trimmed.contains('(')
            && trimmed.contains(')')
            && !trimmed.contains('{')
        {
            return true;
        }
    }
    false
}

fn is_boilerplate_trait_impl(content: &str) -> bool {
    for line in content.lines().take(5) {
        let trimmed = line.trim();
        if trimmed.starts_with("impl") {
            for &t in BOILERPLATE_TRAITS {
                if trimmed.contains(&format!("{t} for")) || trimmed.contains(&format!("{t}<")) {
                    return true;
                }
            }
        }
    }
    false
}

/// Service initialization methods (constructors, not use cases).
fn is_service_init(entity_name: &str, content: &str) -> bool {
    let init_names = ["init", "new", "try_new", "from_config", "build", "setup"];
    if init_names.contains(&entity_name) {
        return true;
    }
    // fn new(...) -> Self or fn init(...) -> Self patterns
    for pat in &init_names {
        if content.contains(&format!("fn {pat}(")) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(content: &str) -> ChunkData {
        ChunkData {
            content: content.to_string(),
            file_path: String::new(),
            chunk_type: String::new(),
            entity_name: String::new(),
        }
    }

    fn chunk_with(
        content: &str,
        file_path: &str,
        chunk_type: &str,
        entity_name: &str,
    ) -> ChunkData {
        ChunkData {
            content: content.to_string(),
            file_path: file_path.to_string(),
            chunk_type: chunk_type.to_string(),
            entity_name: entity_name.to_string(),
        }
    }

    fn primary(c: &ChunkData) -> Option<String> {
        classify_chunk(c).primary_label
    }

    fn layer(c: &ChunkData) -> Option<String> {
        classify_chunk(c).layer
    }

    fn uses(c: &ChunkData) -> Vec<String> {
        classify_chunk(c).uses
    }

    // ---- entity ----

    #[test]
    fn entity_by_derive() {
        let c = chunk("#[derive(EsEntity)]\npub struct Foo {}");
        assert_eq!(primary(&c), Some("entity".to_string()));
    }

    #[test]
    fn entity_by_field_type() {
        let c = chunk_with(
            "pub struct Foo {\n  events: EntityEvents<FooEvent>,\n}",
            "src/entity.rs",
            "struct_item",
            "Foo",
        );
        assert_eq!(primary(&c), Some("entity".to_string()));
    }

    #[test]
    fn entity_not_by_file_alone() {
        let c = chunk_with(
            "pub struct Helper {}",
            "src/entity.rs",
            "struct_item",
            "Helper",
        );
        // Falls back to value_object (struct not classified)
        assert_eq!(primary(&c), Some("value_object".to_string()));
    }

    // ---- entity_event ----

    #[test]
    fn entity_event_by_derive() {
        let c = chunk("#[derive(EsEvent)]\npub enum FooEvent {}");
        assert_eq!(primary(&c), Some("entity_event".to_string()));
    }

    #[test]
    fn entity_event_by_file_pattern() {
        let c = chunk_with(
            "#[serde(tag = \"type\")]\npub enum FooEvent {}",
            "src/entity.rs",
            "enum_item",
            "FooEvent",
        );
        assert_eq!(primary(&c), Some("entity_event".to_string()));
    }

    // ---- error ----

    #[test]
    fn error_by_file() {
        let c = chunk_with(
            "pub enum MyError {}",
            "src/error.rs",
            "enum_item",
            "MyError",
        );
        assert_eq!(primary(&c), Some("error".to_string()));
    }

    // ---- repository ----

    #[test]
    fn repository_by_derive() {
        let c = chunk("#[derive(EsRepo)]\npub struct FooRepo {}");
        assert_eq!(primary(&c), Some("repository".to_string()));
    }

    #[test]
    fn repository_by_name() {
        let c = chunk_with(
            "pub struct FooRepo {}",
            "src/repo.rs",
            "struct_item",
            "FooRepo",
        );
        assert_eq!(primary(&c), Some("repository".to_string()));
    }

    // ---- service ----

    #[test]
    fn service_by_repo_field_in_mod() {
        let c = chunk_with(
            "pub struct Customers {\n  repo: CustomerRepo,\n}",
            "src/customer/mod.rs",
            "struct_item",
            "Customers",
        );
        assert_eq!(primary(&c), Some("service".to_string()));
    }

    #[test]
    fn service_with_arc_deps() {
        let c = chunk_with(
            "pub struct CoreCredit {\n  facilities: Arc<CreditFacilities>,\n  disbursals: Arc<Disbursals>,\n  collections: Arc<Collections>,\n  ledger: Arc<CreditLedger>,\n}",
            "src/lib.rs",
            "struct_item",
            "CoreCredit",
        );
        assert_eq!(primary(&c), Some("service".to_string()));
    }

    #[test]
    fn service_external_with_client() {
        let c = chunk_with(
            "pub struct PriceService {\n  client: reqwest::Client,\n}",
            "src/price/mod.rs",
            "struct_item",
            "PriceService",
        );
        assert_eq!(primary(&c), Some("service".to_string()));
    }

    #[test]
    fn service_by_gateway_path() {
        let c = chunk_with(
            "pub struct BankGateway {}",
            "src/gateway.rs",
            "struct_item",
            "BankGateway",
        );
        assert_eq!(primary(&c), Some("service".to_string()));
    }

    // ---- entity_command ----

    #[test]
    fn entity_command_by_idempotency() {
        let c = chunk_with(
            "pub fn create(&mut self) { idempotency_guard!(self); }",
            "src/entity.rs",
            "impl_method",
            "create",
        );
        let cls = classify_chunk(&c);
        assert_eq!(cls.primary_label, Some("entity_command".to_string()));
        assert!(cls.primary_confidence > 0.9);
    }

    #[test]
    fn entity_command_by_events_push() {
        let c = chunk_with(
            "pub fn activate(&mut self) {\n  self.events.push(FooEvent::Activated);\n}",
            "src/entity.rs",
            "impl_method",
            "activate",
        );
        assert_eq!(primary(&c), Some("entity_command".to_string()));
    }

    // ---- entity_query ----

    #[test]
    fn entity_query_by_self_ref() {
        let c = chunk_with(
            "pub fn name(&self) -> &str { &self.name }",
            "src/entity.rs",
            "impl_method",
            "name",
        );
        assert_eq!(primary(&c), Some("entity_query".to_string()));
    }

    #[test]
    fn entity_query_not_outside_entity() {
        let c = chunk_with(
            "pub fn name(&self) -> &str { &self.name }",
            "src/mod.rs",
            "impl_method",
            "name",
        );
        assert_ne!(primary(&c), Some("entity_query".to_string()));
    }

    // ---- service_method (was use_case) ----

    #[test]
    fn service_method_in_op() {
        let c = chunk_with(
            "pub async fn create_in_op(&self, op: &mut DbOp<'_>) {}",
            "src/mod.rs",
            "impl_method",
            "create_in_op",
        );
        assert_eq!(primary(&c), Some("service_method".to_string()));
    }

    #[test]
    fn service_method_standalone() {
        let c = chunk_with(
            "pub async fn create(&self) -> Result<()> {\n  let mut db = self.repo.begin_op().await?;\n}",
            "src/mod.rs",
            "impl_method",
            "create",
        );
        assert_eq!(primary(&c), Some("service_method".to_string()));
    }

    #[test]
    fn service_init_is_service() {
        let c = chunk_with(
            "pub async fn init(pool: &Pool) -> Result<Self> {}",
            "src/mod.rs",
            "impl_method",
            "init",
        );
        assert_eq!(primary(&c), Some("service".to_string()));
    }

    #[test]
    fn service_new_is_service() {
        let c = chunk_with(
            "pub async fn new(repo: Repo) -> Self {}",
            "src/lib.rs",
            "impl_method",
            "new",
        );
        assert_eq!(primary(&c), Some("service".to_string()));
    }

    // ---- value_object ----

    #[test]
    fn value_object_newtype_in_primitives() {
        let c = chunk_with(
            "pub struct Amount(Decimal);",
            "src/primitives.rs",
            "struct_item",
            "Amount",
        );
        // domain_primitives wins over value_object due to priority
        assert_eq!(primary(&c), Some("domain_primitives".to_string()));
    }

    #[test]
    fn value_object_fallback_in_domain() {
        let c = chunk_with(
            "pub struct Metadata { pub key: String }",
            "src/entity.rs",
            "struct_item",
            "Metadata",
        );
        assert_eq!(primary(&c), Some("value_object".to_string()));
    }

    #[test]
    fn no_value_object_fallback_outside_domain() {
        let c = chunk_with(
            "pub struct Address { pub line1: String }",
            "src/wire.rs",
            "struct_item",
            "Address",
        );
        assert_eq!(primary(&c), None);
    }

    // ---- domain_primitives ----

    #[test]
    fn domain_primitives_by_file() {
        let c = chunk_with(
            "pub struct Status(String);",
            "src/primitives.rs",
            "struct_item",
            "Status",
        );
        assert_eq!(primary(&c), Some("domain_primitives".to_string()));
    }

    #[test]
    fn domain_primitives_entity_id_macro() {
        let c = chunk("entity_id! { CustomerId }");
        assert_eq!(primary(&c), Some("domain_primitives".to_string()));
    }

    // ---- new_entity ----

    #[test]
    fn new_entity_detection() {
        let c = chunk_with(
            "#[derive(Builder)]\npub struct NewCustomer {}",
            "src/entity.rs",
            "struct_item",
            "NewCustomer",
        );
        assert_eq!(primary(&c), Some("new_entity".to_string()));
    }

    // ---- entity_hydration ----

    #[test]
    fn entity_hydration_detection() {
        let c = chunk_with(
            "impl TryFromEvents<FooEvent> for Foo {}",
            "src/entity.rs",
            "impl_item",
            "Foo",
        );
        assert_eq!(primary(&c), Some("entity_hydration".to_string()));
    }

    // ---- test ----

    #[test]
    fn test_detection() {
        let c = chunk_with(
            "@test \"it works\" {}",
            "tests/foo.bats",
            "test_block",
            "it works",
        );
        assert_eq!(primary(&c), Some("test".to_string()));
    }

    #[test]
    fn test_unit_test() {
        let c = chunk_with(
            "#[test]\nfn test_add() { assert_eq!(2 + 2, 4); }",
            "src/entity.rs",
            "function_item",
            "test_add",
        );
        assert_eq!(primary(&c), Some("test".to_string()));
    }

    #[test]
    fn test_integration() {
        let c = chunk_with(
            "#[tokio::test]\nasync fn test_scenario() -> anyhow::Result<()> {}",
            "tests/deposit.rs",
            "function_item",
            "test_scenario",
        );
        assert_eq!(primary(&c), Some("test".to_string()));
    }

    // ---- job ----

    #[test]
    fn job_by_initializer() {
        let c = chunk_with(
            "impl JobInitializer for MyJob {\n  type Config = MyConfig;\n}",
            "src/jobs/my_job.rs",
            "impl_item",
            "MyJob",
        );
        assert_eq!(primary(&c), Some("job".to_string()));
    }

    #[test]
    fn job_by_file_path() {
        let c = chunk_with(
            "pub struct MyJob {}",
            "src/jobs/process.rs",
            "struct_item",
            "MyJob",
        );
        assert_eq!(primary(&c), Some("job".to_string()));
    }

    // ---- event_handler ----

    #[test]
    fn event_handler_by_trait() {
        let c = chunk_with(
            "impl OutboxEventHandler<E> for EndOfDayHandler {\n  async fn handle_persistent(&self, op: &mut DbOp<'_>, event: &PersistentOutboxEvent<E>) {}\n}",
            "src/jobs/end_of_day.rs",
            "impl_item",
            "EndOfDayHandler",
        );
        assert_eq!(primary(&c), Some("event_handler".to_string()));
    }

    // ---- published_event ----

    #[test]
    fn published_event_in_public_module() {
        let c = chunk_with(
            "#[derive(Serialize, Deserialize)]\npub enum CoreCreditEvent {\n  FacilityActivated {},\n}",
            "src/public/event.rs",
            "enum_item",
            "CoreCreditEvent",
        );
        assert_eq!(primary(&c), Some("published_event".to_string()));
    }

    #[test]
    fn published_event_by_marker() {
        let c = chunk("impl OutboxEventMarker<CoreCreditEvent> for AppEvent {}");
        assert_eq!(primary(&c), Some("published_event".to_string()));
    }

    #[test]
    fn published_event_publisher_struct() {
        let c = chunk_with(
            "pub struct DepositPublisher<E>\nwhere\n    E: OutboxEventMarker<CoreDepositEvent>,\n{\n    outbox: Outbox<E>,\n}",
            "src/publisher.rs",
            "struct_item",
            "DepositPublisher",
        );
        let cls = classify_chunk(&c);
        assert_eq!(cls.primary_label, Some("published_event".to_string()));
        assert_eq!(cls.layer, Some("infrastructure".to_string()));
    }

    #[test]
    fn published_event_publish_method() {
        let c = chunk_with(
            "// impl DepositPublisher<E>\npub async fn publish_deposit_in_op(\n    &self,\n    op: &mut impl es_entity::AtomicOperation,\n    entity: &Deposit,\n) -> Result<(), sqlx::Error> {\n    self.outbox.publish_all_persisted(op, events).await?;\n    Ok(())\n}",
            "src/publisher.rs",
            "impl_method",
            "publish_deposit_in_op",
        );
        let cls = classify_chunk(&c);
        assert_eq!(cls.primary_label, Some("published_event".to_string()));
        assert!(cls.uses.contains(&"publisher".to_string()));
    }

    #[test]
    fn published_event_publisher_new_excluded() {
        // Constructor in publisher.rs should not be labeled as published_event
        let c = chunk_with(
            "pub fn new(outbox: &Outbox<E>) -> Self {\n    Self { outbox: outbox.clone() }\n}",
            "src/publisher.rs",
            "impl_method",
            "new",
        );
        assert_ne!(primary(&c), Some("published_event".to_string()));
    }

    #[test]
    fn published_event_outbox_event_marker_bound() {
        let c = chunk_with(
            "pub struct Publisher<E>\nwhere\n    E: OutboxEventMarker<MyEvent>,\n{\n    outbox: Outbox<E>,\n}",
            "src/lib.rs",
            "struct_item",
            "Publisher",
        );
        assert_eq!(primary(&c), Some("published_event".to_string()));
    }

    #[test]
    fn published_event_impl_with_marker_bound() {
        let c = chunk_with(
            "// impl Publisher<E>\nimpl<E> Publisher<E>\nwhere\n    E: OutboxEventMarker<MyEvent>,\n{\n    pub async fn publish_in_op(&self, op: &mut impl AtomicOperation) {}\n}",
            "src/publisher.rs",
            "impl_method",
            "publish_in_op",
        );
        assert_eq!(primary(&c), Some("published_event".to_string()));
    }

    #[test]
    fn published_event_public_dto_struct_not_matched() {
        // Public DTO structs in public/ are NOT published events
        let c = chunk_with(
            "#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct PublicDeposit {\n    pub id: DepositId,\n    pub amount: UsdCents,\n}",
            "src/public/deposit.rs",
            "struct_item",
            "PublicDeposit",
        );
        assert_ne!(primary(&c), Some("published_event".to_string()));
    }

    #[test]
    fn published_event_public_from_impl_is_type_conversion() {
        // From impls in public/ should be type_conversion, not published_event
        let c = chunk_with(
            "impl From<&Deposit> for PublicDeposit {\n    fn from(entity: &Deposit) -> Self { todo!() }\n}",
            "src/public/deposit.rs",
            "impl_item",
            "PublicDeposit",
        );
        assert_eq!(primary(&c), Some("type_conversion".to_string()));
    }

    // ---- authorization ----

    #[test]
    fn authorization_by_permission_check() {
        let c = chunk_with(
            "pub async fn create(&self) {\n  self.authz.evaluate_permission(sub, object).await?;\n}",
            "src/mod.rs",
            "impl_method",
            "create",
        );
        // authorization wins over service_method by priority
        assert_eq!(primary(&c), Some("authorization".to_string()));
        assert!(uses(&c).contains(&"authorization".to_string()));
    }

    #[test]
    fn authorization_by_file_path() {
        let c = chunk_with(
            "pub struct AuthzConfig {}",
            "src/authz/config.rs",
            "struct_item",
            "AuthzConfig",
        );
        assert_eq!(primary(&c), Some("authorization".to_string()));
    }

    // ---- api_handler ----

    #[test]
    fn api_handler_graphql() {
        let c = chunk_with(
            "#[derive(SimpleObject)]\npub struct CustomerGql {}",
            "src/graphql/customer.rs",
            "struct_item",
            "CustomerGql",
        );
        assert_eq!(primary(&c), Some("api".to_string()));
    }

    // ---- config ----

    #[test]
    fn config_by_file() {
        let c = chunk_with(
            "#[derive(Deserialize)]\npub struct AppSettings {\n  pub default: String,\n}",
            "src/config.rs",
            "struct_item",
            "AppSettings",
        );
        assert_eq!(primary(&c), Some("config".to_string()));
    }

    // ---- type_conversion ----

    #[test]
    fn type_conversion_from_impl() {
        let c = chunk_with(
            "impl From<DomainError> for ApiError {}",
            "src/convert.rs",
            "impl_item",
            "ApiError",
        );
        assert_eq!(primary(&c), Some("type_conversion".to_string()));
    }

    // ---- priority: most-specific wins ----

    #[test]
    fn priority_entity_hydration_over_entity() {
        let c = chunk_with(
            "impl TryFromEvents<FooEvent> for Foo {\n  fn try_from_events(events: EntityEvents<FooEvent>) -> Result<Self> {}\n}",
            "src/entity.rs",
            "impl_item",
            "Foo",
        );
        assert_eq!(primary(&c), Some("entity_hydration".to_string()));
    }

    #[test]
    fn priority_entity_command_over_entity() {
        let c = chunk_with(
            "pub fn activate(&mut self) {\n  self.events.push(FooEvent::Activated);\n}",
            "src/entity.rs",
            "impl_method",
            "activate",
        );
        assert_eq!(primary(&c), Some("entity_command".to_string()));
    }

    // ---- no-label cases ----

    #[test]
    fn no_label_for_bare_function() {
        let c = chunk_with(
            "fn helper_fn() -> i32 { 42 }",
            "src/utils.rs",
            "function_item",
            "helper_fn",
        );
        assert_eq!(primary(&c), None);
    }

    #[test]
    fn no_label_for_trait_impl() {
        let c = chunk_with(
            "impl Display for MyType {\n  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}",
            "src/types.rs",
            "impl_item",
            "MyType",
        );
        assert_eq!(primary(&c), None);
    }

    // ---- layer detection ----

    #[test]
    fn layer_domain_for_entity() {
        let c = chunk_with("struct Foo {}", "src/entity.rs", "struct_item", "Foo");
        assert_eq!(layer(&c), Some("domain".to_string()));
    }

    #[test]
    fn layer_domain_for_primitives() {
        let c = chunk_with("struct Id(Uuid);", "src/primitives.rs", "struct_item", "Id");
        assert_eq!(layer(&c), Some("domain".to_string()));
    }

    #[test]
    fn layer_domain_for_error() {
        let c = chunk_with("enum Err {}", "src/error.rs", "enum_item", "Err");
        assert_eq!(layer(&c), Some("domain".to_string()));
    }

    #[test]
    fn layer_application_for_mod() {
        let c = chunk_with("struct Svc {}", "src/customer/mod.rs", "struct_item", "Svc");
        assert_eq!(layer(&c), Some("application".to_string()));
    }

    #[test]
    fn layer_infrastructure_for_repo() {
        let c = chunk_with("struct Repo {}", "src/repo.rs", "struct_item", "Repo");
        assert_eq!(layer(&c), Some("infrastructure".to_string()));
    }

    #[test]
    fn layer_infrastructure_for_publisher() {
        let c = chunk_with("struct Pub {}", "src/publisher.rs", "struct_item", "Pub");
        assert_eq!(layer(&c), Some("infrastructure".to_string()));
    }

    #[test]
    fn layer_infrastructure_for_jobs() {
        let c = chunk_with("struct J {}", "src/jobs/foo.rs", "struct_item", "J");
        assert_eq!(layer(&c), Some("infrastructure".to_string()));
    }

    #[test]
    fn layer_interface_for_graphql() {
        let c = chunk_with("struct Q {}", "src/graphql/query.rs", "struct_item", "Q");
        assert_eq!(layer(&c), Some("interface".to_string()));
    }

    #[test]
    fn layer_interface_for_server() {
        let c = chunk_with("struct S {}", "src/server/main.rs", "struct_item", "S");
        assert_eq!(layer(&c), Some("interface".to_string()));
    }

    #[test]
    fn layer_none_for_tests() {
        let c = chunk_with("#[test]\nfn t() {}", "tests/foo.rs", "function_item", "t");
        assert_eq!(layer(&c), None);
    }

    // ---- uses detection ----

    #[test]
    fn uses_repository() {
        let c = chunk("let x = self.repo.find_by_id(id).await?;");
        let u = uses(&c);
        assert!(u.contains(&"repository".to_string()));
    }

    #[test]
    fn uses_events() {
        let c = chunk("self.events.push(MyEvent::Created {});");
        let u = uses(&c);
        assert!(u.contains(&"events".to_string()));
    }

    #[test]
    fn uses_authorization() {
        let c = chunk("self.authz.enforce_permission(subject, action).await?;");
        let u = uses(&c);
        assert!(u.contains(&"authorization".to_string()));
    }

    #[test]
    fn uses_publisher() {
        let c = chunk("self.outbox.publish_all_persisted(op).await?;");
        let u = uses(&c);
        assert!(u.contains(&"publisher".to_string()));
    }

    #[test]
    fn uses_publisher_not_for_field_type() {
        let c = chunk("outbox: Outbox<MyEvent>,");
        let u = uses(&c);
        assert!(!u.contains(&"publisher".to_string()));
    }

    #[test]
    fn uses_config() {
        let c = chunk("let cfg = ExposedDomainConfig::new();");
        let u = uses(&c);
        assert!(u.contains(&"config".to_string()));
    }

    // ---- backward compat shim ----

    #[test]
    fn label_chunk_returns_single_role() {
        let c = chunk("#[derive(EsEntity)]\npub struct Foo {}");
        let roles = label_chunk(&c);
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].role, "entity");
    }

    #[test]
    fn label_chunk_returns_empty_for_no_match() {
        let c = chunk_with(
            "fn helper() -> i32 { 42 }",
            "src/utils.rs",
            "function_item",
            "helper",
        );
        let roles = label_chunk(&c);
        assert!(roles.is_empty());
    }

    // ---- frontend: component ----

    #[test]
    fn component_by_path() {
        let c = chunk_with(
            "export function Button() { return <button>Click</button>; }",
            "src/components/Button.tsx",
            "function_declaration",
            "Button",
        );
        assert_eq!(primary(&c), Some("component".to_string()));
    }

    #[test]
    fn component_by_jsx_return() {
        let c = chunk_with(
            "export function Card({ title }) { return <div>{title}</div>; }",
            "src/shared/Card.tsx",
            "function_declaration",
            "Card",
        );
        assert_eq!(primary(&c), Some("component".to_string()));
    }

    #[test]
    fn component_forward_ref() {
        let c = chunk_with(
            "export const Input = forwardRef((props, ref) => { return <input ref={ref} />; });",
            "src/components/Input.tsx",
            "arrow_function",
            "Input",
        );
        assert_eq!(primary(&c), Some("component".to_string()));
    }

    // ---- frontend: page ----

    #[test]
    fn page_by_pages_dir() {
        let c = chunk_with(
            "export default function Home() { return <div>Home</div>; }",
            "src/pages/index.tsx",
            "function_declaration",
            "Home",
        );
        assert_eq!(primary(&c), Some("page".to_string()));
    }

    #[test]
    fn page_by_app_dir() {
        let c = chunk_with(
            "export default function Page() { return <main>Content</main>; }",
            "src/app/dashboard/page.tsx",
            "function_declaration",
            "Page",
        );
        assert_eq!(primary(&c), Some("page".to_string()));
    }

    // ---- frontend: layout ----

    #[test]
    fn layout_by_dir() {
        let c = chunk_with(
            "export function MainLayout({ children }) { return <div>{children}</div>; }",
            "src/layouts/MainLayout.tsx",
            "function_declaration",
            "MainLayout",
        );
        assert_eq!(primary(&c), Some("layout".to_string()));
    }

    #[test]
    fn layout_by_filename() {
        let c = chunk_with(
            "export default function RootLayout({ children }) { return <html>{children}</html>; }",
            "src/app/layout.tsx",
            "function_declaration",
            "RootLayout",
        );
        assert_eq!(primary(&c), Some("layout".to_string()));
    }

    // ---- frontend: hook ----

    #[test]
    fn hook_by_name() {
        let c = chunk_with(
            "export function useAuth() { const [user, setUser] = useState(null); return { user }; }",
            "src/hooks/useAuth.ts",
            "function_declaration",
            "useAuth",
        );
        assert_eq!(primary(&c), Some("hook".to_string()));
    }

    #[test]
    fn hook_arrow_function() {
        let c = chunk_with(
            "export const useCounter = () => { const [c, set] = useState(0); return c; };",
            "src/hooks/useCounter.ts",
            "arrow_function",
            "useCounter",
        );
        assert_eq!(primary(&c), Some("hook".to_string()));
    }

    #[test]
    fn hook_not_use_lowercase() {
        // "user" starts with "use" but 4th char is lowercase — not a hook
        let c = chunk_with(
            "export function user() { return {}; }",
            "src/lib/user.ts",
            "function_declaration",
            "user",
        );
        assert_ne!(primary(&c), Some("hook".to_string()));
    }

    // ---- frontend: graphql_query ----

    #[test]
    fn graphql_query_by_gql_tag() {
        let c = chunk_with(
            "export const GET_USER = gql`\n  query GetUser($id: ID!) {\n    user(id: $id) { id name }\n  }\n`;",
            "src/graphql/queries.ts",
            "lexical_declaration",
            "GET_USER",
        );
        assert_eq!(primary(&c), Some("graphql_query".to_string()));
    }

    #[test]
    fn graphql_query_by_usequery() {
        let c = chunk_with(
            "export function useUserData(id) { return useQuery(GET_USER, { variables: { id } }); }",
            "src/hooks/useUserData.ts",
            "function_declaration",
            "useUserData",
        );
        // hook takes priority over graphql_query in priority order
        assert_eq!(primary(&c), Some("hook".to_string()));
    }

    // ---- frontend: graphql_mutation ----

    #[test]
    fn graphql_mutation_by_gql_tag() {
        let c = chunk_with(
            "export const CREATE_USER = gql`\n  mutation CreateUser($input: CreateUserInput!) {\n    createUser(input: $input) { id }\n  }\n`;",
            "src/graphql/mutations.ts",
            "lexical_declaration",
            "CREATE_USER",
        );
        assert_eq!(primary(&c), Some("graphql_mutation".to_string()));
    }

    #[test]
    fn graphql_mutation_by_usemutation() {
        let c = chunk_with(
            "export const useCreateUser = () => { const [create] = useMutation(CREATE_USER); return create; };",
            "src/hooks/useCreateUser.ts",
            "arrow_function",
            "useCreateUser",
        );
        // hook wins by priority
        assert_eq!(primary(&c), Some("hook".to_string()));
    }

    // ---- frontend: ui_primitive ----

    #[test]
    fn ui_primitive_by_dir() {
        let c = chunk_with(
            "export function Badge({ text }) { return <span className='badge'>{text}</span>; }",
            "src/ui/Badge.tsx",
            "function_declaration",
            "Badge",
        );
        assert_eq!(primary(&c), Some("ui_primitive".to_string()));
    }

    // ---- frontend: store ----

    #[test]
    fn store_by_create_slice() {
        let c = chunk_with(
            "export const userSlice = createSlice({ name: 'user', initialState: {} });",
            "src/store/userSlice.ts",
            "lexical_declaration",
            "userSlice",
        );
        assert_eq!(primary(&c), Some("store".to_string()));
    }

    #[test]
    fn store_by_entity_name() {
        let c = chunk_with(
            "export const authStore = create((set) => ({ user: null }));",
            "src/stores/auth.ts",
            "lexical_declaration",
            "authStore",
        );
        assert_eq!(primary(&c), Some("store".to_string()));
    }

    // ---- frontend: route_handler ----

    #[test]
    fn route_handler_nextjs_api() {
        let c = chunk_with(
            "export async function GET(request) { return Response.json({ ok: true }); }",
            "src/app/api/users/route.ts",
            "function_declaration",
            "GET",
        );
        assert_eq!(primary(&c), Some("route_handler".to_string()));
    }

    #[test]
    fn route_handler_express() {
        let c = chunk_with(
            "router.get('/users', async (req, res) => { res.json([]); });",
            "src/routes/users.ts",
            "arrow_function",
            "getUsers",
        );
        assert_eq!(primary(&c), Some("route_handler".to_string()));
    }

    // ---- frontend: style_variant ----

    #[test]
    fn style_variant_styled_components() {
        let c = chunk_with(
            "export const StyledButton = styled.button`\n  background: blue;\n  color: white;\n`;",
            "src/components/Button.styles.ts",
            "lexical_declaration",
            "StyledButton",
        );
        assert_eq!(primary(&c), Some("style_variant".to_string()));
    }

    #[test]
    fn style_variant_cva() {
        let c = chunk_with(
            "export const buttonVariants = cva('base-class', { variants: { size: { sm: 'px-2' } } });",
            "src/components/button.variants.ts",
            "lexical_declaration",
            "buttonVariants",
        );
        assert_eq!(primary(&c), Some("style_variant".to_string()));
    }

    // ---- frontend: test detection ----

    #[test]
    fn js_test_by_spec_file() {
        let c = chunk_with(
            "describe('Button', () => { it('renders', () => { expect(true).toBe(true); }); });",
            "src/components/Button.spec.tsx",
            "arrow_function",
            "ButtonSpec",
        );
        assert_eq!(primary(&c), Some("test".to_string()));
    }

    #[test]
    fn js_test_by_test_file() {
        let c = chunk_with(
            "test('adds numbers', () => { expect(add(1, 2)).toBe(3); });",
            "src/utils/math.test.ts",
            "arrow_function",
            "addsNumbers",
        );
        assert_eq!(primary(&c), Some("test".to_string()));
    }

    // ---- frontend: layer detection ----

    #[test]
    fn js_interface_layer_components() {
        let c = chunk_with(
            "export function Button() { return <button/>; }",
            "src/components/Button.tsx",
            "function_declaration",
            "Button",
        );
        assert_eq!(layer(&c), Some("interface".to_string()));
    }

    #[test]
    fn js_infrastructure_layer_api() {
        let c = chunk_with(
            "export async function GET() { return Response.json({}); }",
            "src/app/api/users/route.ts",
            "function_declaration",
            "GET",
        );
        assert_eq!(layer(&c), Some("infrastructure".to_string()));
    }

    #[test]
    fn js_application_layer_hooks() {
        let c = chunk_with(
            "export function useAuth() { return {}; }",
            "src/hooks/useAuth.ts",
            "function_declaration",
            "useAuth",
        );
        assert_eq!(layer(&c), Some("application".to_string()));
    }

    #[test]
    fn js_test_no_layer() {
        let c = chunk_with(
            "describe('test', () => { it('works', () => { expect(1).toBe(1); }); });",
            "src/components/Button.test.tsx",
            "arrow_function",
            "test",
        );
        assert_eq!(layer(&c), None);
    }

    // ---- frontend: non-JS files don't match ----

    #[test]
    fn rust_file_not_component() {
        let c = chunk_with(
            "pub fn Button() -> Html { html! { <button/> } }",
            "src/components/button.rs",
            "function_item",
            "Button",
        );
        // Rust file should not match component checker
        assert_ne!(primary(&c), Some("component".to_string()));
    }
}
