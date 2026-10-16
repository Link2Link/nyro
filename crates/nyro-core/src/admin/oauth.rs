use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct SessionFlight {
    generation: u64,
    busy: Arc<AtomicBool>,
    cancellation: tokio_util::sync::CancellationToken,
}
impl Drop for SessionFlight {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

fn validate_oauth_session_for_provider(
    session: &AuthSession,
    provider: &Provider,
) -> anyhow::Result<()> {
    if provider.effective_auth_mode().trim() != "oauth" {
        anyhow::bail!("target provider is not configured for OAuth");
    }

    let expected_driver = provider
        .vendor
        .as_deref()
        .map(auth::normalize_driver_key)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            provider
                .preset_key
                .as_deref()
                .map(auth::normalize_driver_key)
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| anyhow::anyhow!("target provider has no OAuth vendor"))?;
    // The google vendor carries two subscription channels with distinct
    // OAuth clients; the channel picks the driver.
    let expected_driver = if expected_driver == "google"
        && provider
            .channel
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("gemini-cli"))
    {
        "google-gemini-cli".to_string()
    } else {
        expected_driver
    };
    let session_driver = auth::normalize_driver_key(&session.driver_key);
    if session_driver != expected_driver {
        anyhow::bail!(
            "OAuth session driver '{}' is incompatible with provider driver '{}'",
            session.driver_key,
            expected_driver
        );
    }

    let driver = auth::build_driver(&session_driver)
        .ok_or_else(|| anyhow::anyhow!("auth vendor not implemented: {session_driver}"))?;
    if !driver.metadata().supports_existing_provider && !provider.id.is_empty() {
        anyhow::bail!(
            "auth vendor does not support binding an existing provider: {session_driver}"
        );
    }
    let expected_scheme = driver.metadata().scheme.as_str();
    if session.scheme.trim() != expected_scheme {
        anyhow::bail!(
            "OAuth session scheme '{}' is incompatible with provider driver '{}'",
            session.scheme,
            session_driver
        );
    }

    if session_driver == "devin"
        && (!matches!(
            provider.protocol.as_str(),
            "devin-chat" | "devin-chat/get-chat-message/v1"
        ) || provider.channel.as_deref() != Some("cli")
            || provider.preset_key.as_deref() != Some("devin")
            || provider.protocol_mode != "fixed"
            || provider.base_url.trim_end_matches('/') != "https://server.codeium.com")
    {
        anyhow::bail!("Devin OAuth session can only bind to the official devin/cli channel");
    }
    if session_driver == "codex"
        && (provider.protocol.trim() != "openai-responses"
            || !provider
                .channel
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("codex"))
            || !provider
                .preset_key
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("openai")))
    {
        anyhow::bail!("Codex OAuth session can only bind to the openai/codex channel");
    }
    if session_driver == "grok"
        && (provider.protocol.trim() != "openai-responses"
            || !provider
                .channel
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("grok"))
            || !provider
                .preset_key
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("xai")))
    {
        anyhow::bail!("Grok OAuth session can only bind to the xai/grok channel");
    }
    if session_driver == "google"
        && (provider.protocol.trim() != "google-gemini"
            || !provider
                .channel
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("antigravity"))
            || !provider
                .preset_key
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("google")))
    {
        anyhow::bail!("Google OAuth session can only bind to the google/antigravity channel");
    }
    if session_driver == "google-gemini-cli"
        && (provider.protocol.trim() != "google-gemini"
            || !provider
                .channel
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("gemini-cli"))
            || !provider
                .preset_key
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("google")))
    {
        anyhow::bail!("Gemini CLI OAuth session can only bind to the google/gemini-cli channel");
    }
    Ok(())
}

impl AdminService {
    pub async fn init_oauth_session(
        &self,
        vendor: &str,
        use_proxy: bool,
    ) -> anyhow::Result<AuthSessionInitData> {
        let driver_key = auth::normalize_driver_key(vendor);
        if driver_key.is_empty() {
            anyhow::bail!("auth vendor cannot be empty");
        }
        let driver = auth::build_driver(&driver_key)
            .ok_or_else(|| anyhow::anyhow!("auth vendor not implemented: {driver_key}"))?;
        let client = if driver_key == "devin" {
            self.gw.devin_http_client(use_proxy, false).await?
        } else {
            self.gw.http_client_for_provider(use_proxy).await?
        };
        let mut created = driver
            .start(StartAuthContext {
                use_proxy,
                http_client: Some(client),
                ..Default::default()
            })
            .await?;
        let mut context = created
            .context_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        context.insert(
            "completion_mode".into(),
            serde_json::to_value(driver.completion_mode())?,
        );
        created.context_json = Some(serde_json::to_string(&context)?);
        // A login session is not a credential: never inherit token lifetime.
        created.expires_at = Some(auth::drivers::expires_at_after(300));
        let session = self.create_auth_session_record(created).await?;
        build_auth_session_init_data(&session)
    }

    pub async fn get_oauth_session_status(
        &self,
        session_id: &str,
    ) -> anyhow::Result<AuthSessionStatusData> {
        let session = self
            .get_auth_session_record(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {session_id}"))?;

        if is_expired_at(session.expires_at.as_deref()) {
            self.delete_auth_session_record(&session.id).await?;
            return Ok(AuthSessionStatusData::Error {
                code: "AUTH_TIMEOUT".to_string(),
                message: "auth session expired".to_string(),
            });
        }

        match session.status.as_str() {
            "ready" => {
                let bundle = parse_auth_session_bundle(&session)?;
                return Ok(build_auth_session_ready_data(&session, &bundle));
            }
            "error" => {
                let message = session
                    .last_error
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "auth session failed".to_string());
                self.delete_auth_session_record(&session.id).await?;
                return Ok(AuthSessionStatusData::Error {
                    code: "AUTH_SESSION_ERROR".to_string(),
                    message,
                });
            }
            "cancelled" => {
                self.delete_auth_session_record(&session.id).await?;
                return Ok(AuthSessionStatusData::Error {
                    code: "AUTH_SESSION_CANCELLED".to_string(),
                    message: "auth session cancelled".to_string(),
                });
            }
            _ => {}
        }

        if session_completion_mode(&session) == auth::AuthCompletionMode::ManualExchange {
            return Ok(build_auth_session_pending_data(&session));
        }

        let driver = auth::build_driver(&session.driver_key).ok_or_else(|| {
            anyhow::anyhow!("auth vendor not implemented: {}", session.driver_key)
        })?;
        let Some(flight) = self.begin_session_flight(&session.id, true).await? else {
            return Ok(build_auth_session_pending_data(&session));
        };
        let operation = async {
            let client = if session.driver_key == "devin" {
                self.gw
                    .devin_http_client(session.use_proxy, false)
                    .await?
            } else {
                self.gw.http_client_for_provider(session.use_proxy).await?
            };
            driver
                .poll(
                    &session,
                    RefreshAuthContext {
                        use_proxy: session.use_proxy,
                        http_client: Some(client),
                        ..Default::default()
                    },
                )
                .await
        };
        let result = tokio::select! {
            _ = flight.cancellation.cancelled() => anyhow::bail!("auth session cancelled"),
            result = operation => result,
        };
        match result {
            Ok(AuthPollState::Pending(progress)) => {
                let updated = self
                    .publish_session_progress(
                        &session.id,
                        &flight,
                        UpdateAuthSession {
                            user_code: progress.user_code,
                            verification_uri: progress.verification_uri,
                            verification_uri_complete: progress.verification_uri_complete,
                            // A driver may not extend the login deadline.
                            poll_interval_seconds: progress.poll_interval_seconds,
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(build_auth_session_pending_data(&updated))
            }
            Ok(AuthPollState::Ready(bundle)) => {
                let updated = self
                    .publish_session_progress(
                        &session.id,
                        &flight,
                        UpdateAuthSession {
                            status: Some("ready".into()),
                            result_json: Some(serde_json::to_string(&bundle)?),
                            expires_at: Some(auth::drivers::expires_at_after(120)),
                            last_error: Some(String::new()),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(build_auth_session_ready_data(&updated, &bundle))
            }
            Ok(AuthPollState::Error { code, message }) => {
                self.delete_auth_session_record(&session.id).await?;
                Ok(AuthSessionStatusData::Error { code, message })
            }
            Err(error) => Err(error),
        }
    }

    pub async fn cancel_oauth_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.delete_auth_session_record(session_id).await
    }

    pub async fn complete_oauth_session(
        &self,
        session_id: &str,
        input: auth::AuthExchangeInput,
    ) -> anyhow::Result<AuthSessionStatusData> {
        let session = self
            .get_auth_session_record(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {session_id}"))?;
        if !session
            .status
            .eq_ignore_ascii_case(AuthSessionStatus::Pending.as_str())
        {
            anyhow::bail!("auth session is not pending");
        }
        if is_expired_at(session.expires_at.as_deref()) {
            self.delete_auth_session_record(&session.id).await?;
            anyhow::bail!("auth session expired");
        }

        let driver = auth::build_driver(&session.driver_key).ok_or_else(|| {
            anyhow::anyhow!("auth vendor not implemented: {}", session.driver_key)
        })?;

        if session_completion_mode(&session) != auth::AuthCompletionMode::ManualExchange {
            anyhow::bail!("auth session requires polling, not manual exchange");
        }
        let flight = self
            .begin_session_flight(&session.id, false)
            .await?
            .ok_or_else(|| anyhow::anyhow!("auth session completion already in progress"))?;
        let operation = async {
            let client = if session.driver_key == "devin" {
                self.gw
                    .devin_http_client(session.use_proxy, false)
                    .await?
            } else {
                self.gw.http_client_for_provider(session.use_proxy).await?
            };
            driver
                .exchange(
                    &session,
                    input,
                    ExchangeAuthContext {
                        use_proxy: session.use_proxy,
                        http_client: Some(client),
                        ..Default::default()
                    },
                )
                .await
        };
        let bundle = tokio::select! {
            _ = flight.cancellation.cancelled() => anyhow::bail!("auth session cancelled"),
            result = operation => result?,
        };
        let updated = self
            .publish_session_progress(
                &session.id,
                &flight,
                UpdateAuthSession {
                    status: Some("ready".into()),
                    result_json: Some(serde_json::to_string(&bundle)?),
                    expires_at: Some(auth::drivers::expires_at_after(120)),
                    last_error: Some(String::new()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(build_auth_session_ready_data(&updated, &bundle))
    }

    async fn begin_session_flight(
        &self,
        id: &str,
        poll: bool,
    ) -> anyhow::Result<Option<SessionFlight>> {
        let mut controls = self.gw.auth_session_controls.lock().await;
        let mut sessions = self.gw.auth_sessions.write().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found"))?;
        if session.status != "pending" || is_expired_at(session.expires_at.as_deref()) {
            anyhow::bail!("auth session is not pending or expired");
        }
        let control = controls.entry(id.to_string()).or_default();
        if control.cancelled {
            anyhow::bail!("auth session cancelled");
        }
        if control.in_flight.load(Ordering::Acquire)
            || (poll
                && control
                    .next_poll_at
                    .is_some_and(|at| at > tokio::time::Instant::now()))
        {
            return Ok(None);
        }
        control.in_flight.store(true, Ordering::Release);
        control.generation += 1;
        session.generation = control.generation;
        Ok(Some(SessionFlight {
            generation: control.generation,
            busy: control.in_flight.clone(),
            cancellation: control.cancellation.clone(),
        }))
    }

    async fn publish_session_progress(
        &self,
        id: &str,
        flight: &SessionFlight,
        input: UpdateAuthSession,
    ) -> anyhow::Result<AuthSession> {
        let mut controls = self.gw.auth_session_controls.lock().await;
        let control = controls
            .get_mut(id)
            .filter(|c| !c.cancelled && c.generation == flight.generation)
            .ok_or_else(|| {
                anyhow::anyhow!("auth session completion was cancelled or superseded")
            })?;
        let current = self
            .get_auth_session_record(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("auth session not found"))?;
        if is_expired_at(current.expires_at.as_deref()) {
            anyhow::bail!("auth session expired");
        }
        let interval = input
            .poll_interval_seconds
            .unwrap_or(current.poll_interval_seconds.unwrap_or(2))
            .max(0);
        control.next_poll_at =
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(interval as u64));
        let updated = self.update_auth_session_record(id, input).await?;
        if updated.status == "ready" {
            control.retain_until =
                tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        }
        Ok(updated)
    }

    async fn create_auth_session_record(
        &self,
        input: auth::CreateAuthSession,
    ) -> anyhow::Result<AuthSession> {
        // NOTE (multi-replica): auth_sessions is an in-process HashMap. In a
        // multi-replica deployment the OAuth callback HTTP request must reach
        // the same replica that initiated the session (sticky session /
        // session-affinity). A future improvement is to persist sessions in the
        // shared DB, but for now callers must ensure session affinity.
        if !self.gw.config.config_poll_interval.is_zero() {
            tracing::debug!(
                "creating oauth session in multi-replica mode \
                 — ensure the callback reaches this replica (session affinity required)"
            );
        }
        let now = now_rfc3339();
        let session = AuthSession {
            generation: 0,
            id: uuid::Uuid::new_v4().to_string(),
            provider_id: input.provider_id,
            driver_key: input.driver_key,
            scheme: input.scheme,
            status: input.status,
            use_proxy: input.use_proxy,
            user_code: input.user_code,
            verification_uri: input.verification_uri,
            verification_uri_complete: input.verification_uri_complete,
            state_json: input.state_json,
            context_json: input.context_json,
            result_json: input.result_json,
            expires_at: input.expires_at,
            poll_interval_seconds: input.poll_interval_seconds,
            last_error: input.last_error,
            created_at: now.clone(),
            updated_at: now,
        };
        let mut controls = self.gw.auth_session_controls.lock().await;
        controls.insert(
            session.id.clone(),
            auth::AuthSessionControl {
                next_poll_at: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(2)),
                ..Default::default()
            },
        );
        self.gw
            .auth_sessions
            .write()
            .await
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub(super) async fn get_auth_session_record(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<AuthSession>> {
        Ok(self.gw.auth_sessions.read().await.get(id).cloned())
    }

    pub(super) async fn take_ready_auth_session_record(
        &self,
        id: &str,
    ) -> anyhow::Result<AuthSession> {
        let mut controls = self.gw.auth_session_controls.lock().await;
        if controls.get(id).is_some_and(|control| control.cancelled) {
            anyhow::bail!("auth session cancelled");
        }
        let mut sessions = self.gw.auth_sessions.write().await;
        let mut session = sessions
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        if !session
            .status
            .eq_ignore_ascii_case(AuthSessionStatus::Ready.as_str())
        {
            sessions.insert(id.to_string(), session);
            anyhow::bail!("auth session is not ready");
        }
        let control = controls.entry(id.to_string()).or_default();
        control.generation += 1;
        session.generation = control.generation;
        Ok(session)
    }

    pub(super) async fn update_auth_session_record(
        &self,
        id: &str,
        input: UpdateAuthSession,
    ) -> anyhow::Result<AuthSession> {
        let mut sessions = self.gw.auth_sessions.write().await;
        let current = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("auth session not found: {id}"))?;
        if let Some(value) = input.status {
            current.status = value;
        }
        if let Some(value) = input.user_code {
            current.user_code = Some(value);
        }
        if let Some(value) = input.verification_uri {
            current.verification_uri = Some(value);
        }
        if let Some(value) = input.verification_uri_complete {
            current.verification_uri_complete = Some(value);
        }
        if let Some(value) = input.state_json {
            current.state_json = Some(value);
        }
        if let Some(value) = input.context_json {
            current.context_json = Some(value);
        }
        if let Some(value) = input.result_json {
            current.result_json = Some(value);
        }
        if let Some(value) = input.expires_at {
            current.expires_at = Some(value);
        }
        if let Some(value) = input.poll_interval_seconds {
            current.poll_interval_seconds = Some(value);
        }
        if let Some(value) = input.last_error {
            current.last_error = Some(value);
        }
        current.updated_at = now_rfc3339();
        Ok(current.clone())
    }

    async fn delete_auth_session_record(&self, id: &str) -> anyhow::Result<()> {
        let mut controls = self.gw.auth_session_controls.lock().await;
        if let Some(control) = controls.get_mut(id) {
            control.cancelled = true;
            control.generation += 1;
            control.cancellation.cancel();
        }
        self.gw.auth_sessions.write().await.remove(id);
        Ok(())
    }

    pub(super) async fn restore_auth_session_record(
        &self,
        mut session: AuthSession,
    ) -> anyhow::Result<()> {
        let controls = self.gw.auth_session_controls.lock().await;
        if is_expired_at(session.expires_at.as_deref())
            || !controls.get(&session.id).is_some_and(|control| {
                !control.cancelled && control.generation == session.generation
            })
        {
            return Ok(());
        }
        session.updated_at = now_rfc3339();
        self.gw
            .auth_sessions
            .write()
            .await
            .entry(session.id.clone())
            .or_insert(session);
        Ok(())
    }

    pub(crate) async fn cleanup_auth_sessions(&self) -> anyhow::Result<usize> {
        let mut controls = self.gw.auth_session_controls.lock().await;
        let mut sessions = self.gw.auth_sessions.write().await;
        let before = sessions.len();
        sessions.retain(|id, session| {
            let keep = !matches!(session.status.as_str(), "error" | "cancelled")
                && !is_expired_at(session.expires_at.as_deref());
            if !keep && let Some(control) = controls.get_mut(id) {
                control.cancelled = true;
                control.generation += 1;
                control.cancellation.cancel();
            }
            keep
        });
        controls.retain(|id, control| {
            sessions.contains_key(id) || control.retain_until > tokio::time::Instant::now()
        });
        Ok(before.saturating_sub(sessions.len()))
    }
    pub async fn create_provider_with_oauth_session(
        &self,
        session_id: &str,
        mut input: CreateProvider,
    ) -> anyhow::Result<Provider> {
        let session = self.take_ready_auth_session_record(session_id).await?;
        if is_expired_at(session.expires_at.as_deref()) {
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("auth session expired");
        }

        let bundle = match parse_auth_session_bundle(&session) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };
        if bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .is_none()
        {
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("auth session missing access token");
        }

        let session_driver = auth::normalize_driver_key(&session.driver_key);
        if input.vendor.as_deref().unwrap_or("").trim().is_empty() {
            input.vendor = Some(match session_driver.as_str() {
                "codex" => "openai".to_string(),
                "grok" => "xai".to_string(),
                _ => session.driver_key.clone(),
            });
        }
        if session_driver == "devin" {
            input.vendor = Some("devin".into());
            input.preset_key = Some("devin".into());
            input.channel = Some("cli".into());
            input.protocol = "devin-chat/get-chat-message/v1".into();
            input.protocol_mode = "fixed".into();
            input.protocol_endpoints.clear();
            input.base_url = "https://server.codeium.com".into();
        }
        if session_driver == "codex" {
            input.preset_key = Some("openai".to_string());
            input.channel = Some("codex".to_string());
            input.protocol = "openai-responses".to_string();
            input.base_url = "https://chatgpt.com/backend-api/codex".to_string();
        }
        if session_driver == "grok" {
            input.preset_key = Some("xai".to_string());
            input.channel = Some("grok".to_string());
            input.protocol = "openai-responses".to_string();
            input.base_url = "https://cli-chat-proxy.grok.com/v1".to_string();
        }
        if session_driver == "google" {
            input.vendor = Some("google".to_string());
            input.preset_key = Some("google".to_string());
            input.channel = Some("antigravity".to_string());
            input.protocol = "google-gemini".to_string();
            input.base_url = "https://cloudcode-pa.googleapis.com".to_string();
            // The OAuth runtime binding owns auth/headers; adaptive
            // multi-endpoint mode would reset it per request.
            input.protocol_mode = "fixed".to_string();
        }
        if session_driver == "google-gemini-cli" {
            input.vendor = Some("google".to_string());
            input.preset_key = Some("google".to_string());
            input.channel = Some("gemini-cli".to_string());
            input.protocol = "google-gemini".to_string();
            input.base_url = "https://cloudcode-pa.googleapis.com".to_string();
            input.protocol_mode = "fixed".to_string();
        }
        input.auth_mode = "oauth".to_string();
        let candidate = Provider {
            id: String::new(),
            name: input.name.clone(),
            vendor: input.vendor.clone(),
            protocol: input.protocol.clone(),
            base_url: input.base_url.clone(),
            protocol_mode: input.protocol_mode.clone(),
            protocol_endpoints: Vec::new(),
            keys: Vec::new(),
            preset_key: input.preset_key.clone(),
            channel: input.channel.clone(),
            models_source: input.models_source.clone(),
            static_models: input.static_models.clone(),
            api_key: String::new(),
            auth_mode: "oauth".to_string(),
            use_proxy: input.use_proxy,
            fast_mode: input.fast_mode,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        if let Err(error) = validate_oauth_session_for_provider(&session, &candidate) {
            self.restore_auth_session_record(session).await?;
            return Err(error);
        }

        let provider = match self.create_provider(input).await {
            Ok(provider) => provider,
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };

        let credential_input =
            upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let provisioned = async {
            let controls = self.gw.auth_session_controls.lock().await;
            if !controls
                .get(&session.id)
                .is_some_and(|c| !c.cancelled && c.generation == session.generation)
            {
                anyhow::bail!("auth session was cancelled or superseded");
            }
            self.gw
                .storage
                .oauth_credentials()
                .upsert(&provider.id, credential_input)
                .await?;
            let credential =
                stored_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
            self.sync_provider_runtime_fields(&provider, &credential)
                .await
        }
        .await;

        let provider = match provisioned {
            Ok(provider) => provider,
            Err(error) => {
                if let Err(cleanup_error) = self.delete_provider(&provider.id).await {
                    tracing::warn!(
                        "failed to rollback oauth provider {} after provisioning error: {}",
                        provider.id,
                        cleanup_error
                    );
                }
                self.restore_auth_session_record(session).await?;
                return Err(error.context("create oauth provider"));
            }
        };

        self.bump_config_epoch().await?;
        self.gw.quota_registry.invalidate(&provider.id);
        Ok(provider)
    }

    pub async fn get_provider_oauth_status(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let driver_key = provider
            .vendor
            .as_deref()
            .map(auth::normalize_driver_key)
            .unwrap_or_default();

        if driver_key.is_empty() {
            return Ok(build_provider_oauth_status(&provider, "", None, None));
        }

        let oauth_cred = self.gw.storage.oauth_credentials().get(id).await?;
        match oauth_cred {
            Some(cred) => Ok(build_provider_oauth_status_from_credential(
                &provider,
                &driver_key,
                &cred,
            )),
            None => Ok(build_provider_oauth_status(
                &provider,
                &driver_key,
                None,
                None,
            )),
        }
    }

    pub async fn reconnect_provider_oauth(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let driver_key = provider
            .vendor
            .as_deref()
            .map(auth::normalize_driver_key)
            .unwrap_or_default();

        if driver_key.is_empty() {
            anyhow::bail!("provider vendor is empty");
        }
        let driver = auth::build_driver(&driver_key)
            .ok_or_else(|| anyhow::anyhow!("auth vendor not implemented: {driver_key}"))?;
        if !driver.metadata().supports_existing_provider {
            anyhow::bail!("auth vendor does not support reconnect: {driver_key}");
        }

        let oauth_cred = self
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider oauth credential not found"))?;
        let oauth_store = self.gw.storage.oauth_credentials();
        let Some(refresh_lock) = oauth_store
            .try_begin_refresh(&provider.id, oauth_cred.status_version)
            .await?
        else {
            anyhow::bail!("provider OAuth credential is already being refreshed");
        };

        let credential = stored_credential_from_oauth(&oauth_cred, &driver_key);
        let bundle = self
            .refresh_locked_credential(&provider, &credential, &refresh_lock, driver.as_ref())
            .await?;
        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        let refreshed_provider = self
            .sync_provider_runtime_fields(&provider, &refreshed_credential)
            .await?;
        self.bump_config_epoch().await?;
        self.gw.quota_registry.invalidate(&provider.id);

        Ok(build_provider_oauth_status(
            &refreshed_provider,
            &driver_key,
            Some(AuthBindingStatus::Connected.as_str().to_string()),
            None,
        ))
    }

    pub async fn logout_provider_oauth(&self, id: &str) -> anyhow::Result<ProviderOAuthStatusData> {
        let provider = self.get_provider(id).await?;
        let driver_key = provider
            .vendor
            .as_deref()
            .map(auth::normalize_driver_key)
            .unwrap_or_default();

        if driver_key.is_empty() {
            return Ok(build_provider_oauth_status(&provider, "", None, None));
        }

        self.gw
            .storage
            .oauth_credentials()
            .delete(&provider.id)
            .await?;

        let updated = self
            .gw
            .storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    auth_mode: Some("oauth".to_string()),
                    api_key: Some(String::new()),
                    ..Default::default()
                },
            )
            .await?;
        self.bump_config_epoch().await?;
        self.gw.quota_registry.invalidate(&provider.id);

        Ok(build_provider_oauth_status(
            &updated,
            &driver_key,
            Some(AuthBindingStatus::Disconnected.as_str().to_string()),
            None,
        ))
    }

    pub async fn bind_provider_with_oauth_session(
        &self,
        provider_id: &str,
        session_id: &str,
    ) -> anyhow::Result<Provider> {
        let provider = self.get_provider(provider_id).await?;
        let session = self.take_ready_auth_session_record(session_id).await?;
        if is_expired_at(session.expires_at.as_deref()) {
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("auth session expired");
        }
        if let Err(error) = validate_oauth_session_for_provider(&session, &provider) {
            self.restore_auth_session_record(session).await?;
            return Err(error);
        }

        let bundle = match parse_auth_session_bundle(&session) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };
        if bundle
            .access_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .is_none()
        {
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("auth session missing access token");
        }

        let credential =
            stored_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let credential_input =
            upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let oauth_store = self.gw.storage.oauth_credentials();
        let previous_credential = oauth_store.get(&provider.id).await?;
        let published = async {
            let controls = self.gw.auth_session_controls.lock().await;
            if !controls
                .get(&session.id)
                .is_some_and(|c| !c.cancelled && c.generation == session.generation)
            {
                anyhow::bail!("auth session was cancelled or superseded");
            }
            oauth_store.upsert(&provider.id, credential_input).await
        }
        .await;
        let published = match published {
            Ok(published) => published,
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };
        let provider = match self
            .sync_provider_runtime_fields(&provider, &credential)
            .await
        {
            Ok(provider) => provider,
            Err(error) => {
                // A logout/rebind after our publish is authoritative. Never
                // resurrect the snapshot using an unconditional upsert/delete.
                let rollback = oauth_store
                    .restore_if_version(
                        &provider.id,
                        published.status_version,
                        previous_credential
                            .as_ref()
                            .map(upsert_credential_from_oauth),
                    )
                    .await;
                if let Err(rollback_error) = rollback {
                    tracing::error!(
                        provider_id = %provider.id,
                        error = %rollback_error,
                        "failed to restore prior OAuth credential after bind failure"
                    );
                }
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };

        self.bump_config_epoch().await?;
        self.gw.quota_registry.invalidate(&provider.id);
        Ok(provider)
    }
    pub(crate) async fn resolve_provider_runtime(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<ResolvedProviderRuntime> {
        if provider.effective_auth_mode().trim() != "oauth" {
            let api_key = provider.api_key.trim().to_string();
            if api_key.is_empty() {
                anyhow::bail!("provider api key is empty");
            }
            if vertexai::is_vertex_vendor(provider) {
                let access_token = vertexai::vertex_access_token(&api_key).await?;
                let binding = RuntimeBinding {
                    base_url_override: Some(vertexai::expand_vertex_base_url(
                        &provider.base_url,
                        &api_key,
                    )),
                    ..RuntimeBinding::default()
                };
                return Ok(ResolvedProviderRuntime {
                    access_token,
                    binding,
                    credential: None,
                });
            }
            return Ok(ResolvedProviderRuntime {
                access_token: api_key,
                binding: RuntimeBinding::default(),
                credential: None,
            });
        }

        let oauth_cred = self
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?;

        let oauth_cred = match oauth_cred {
            Some(c) => c,
            None => anyhow::bail!("provider oauth credential not found"),
        };

        let driver_key = if oauth_cred.driver_key.is_empty() {
            provider
                .vendor
                .as_deref()
                .map(auth::normalize_driver_key)
                .unwrap_or_default()
        } else {
            oauth_cred.driver_key.clone()
        };

        let credential = stored_credential_from_oauth(&oauth_cred, &driver_key);
        let access_token = credential
            .access_token
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();

        if !access_token.is_empty() && !is_expired_at(credential.expires_at.as_deref()) {
            let binding = if let Some(driver) = auth::build_driver(&driver_key) {
                driver.bind_runtime(provider, &credential)?
            } else {
                RuntimeBinding::default()
            };
            return Ok(ResolvedProviderRuntime {
                access_token,
                binding,
                credential: Some(credential.clone()),
            });
        }

        let refresh_token = credential
            .refresh_token
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        if refresh_token.is_empty() {
            anyhow::bail!("provider oauth refresh token is missing; reauthentication required");
        }

        let Some(driver) = auth::build_driver(&driver_key) else {
            anyhow::bail!("no auth driver found for key: {driver_key}");
        };

        // CAS lock: transition connected → refreshing to prevent concurrent refresh
        let oauth_store = self.gw.storage.oauth_credentials();
        let locked = oauth_store
            .try_begin_refresh(&provider.id, oauth_cred.status_version)
            .await?;
        let Some(refresh_lock) = locked else {
            // Another caller or replica won the CAS. Wait briefly for it to
            // publish a connected, unexpired token; never reuse the old expired
            // value that caused this refresh attempt.
            const ATTEMPTS: usize = 20;
            const DELAY: std::time::Duration = std::time::Duration::from_millis(100);
            for attempt in 0..ATTEMPTS {
                let refreshed = oauth_store.get(&provider.id).await?.ok_or_else(|| {
                    anyhow::anyhow!("provider oauth credential disappeared during refresh")
                })?;
                if refreshed.status == "connected"
                    && !refreshed.access_token.trim().is_empty()
                    && !is_expired_at(refreshed.expires_at.as_deref())
                {
                    let refreshed_token = refreshed.access_token.trim().to_string();
                    let cred = stored_credential_from_oauth(&refreshed, &driver_key);
                    let binding = driver.bind_runtime(provider, &cred)?;
                    return Ok(ResolvedProviderRuntime {
                        access_token: refreshed_token,
                        binding,
                        credential: Some(cred),
                    });
                }
                if refreshed.status == "error" {
                    anyhow::bail!(
                        "concurrent OAuth refresh failed: {}",
                        refreshed
                            .last_error
                            .as_deref()
                            .unwrap_or("unknown refresh error")
                    );
                }
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(DELAY).await;
                }
            }
            anyhow::bail!("concurrent OAuth refresh did not publish a valid token in time");
        };

        let bundle = self
            .refresh_locked_credential(provider, &credential, &refresh_lock, driver.as_ref())
            .await?;
        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        let refreshed_provider = self
            .sync_provider_runtime_fields(provider, &refreshed_credential)
            .await?;
        self.bump_config_epoch().await?;
        let new_access_token = bundle
            .access_token
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| {
                anyhow::anyhow!("provider credential refresh returned empty access token")
            })?;

        Ok(ResolvedProviderRuntime {
            access_token: new_access_token,
            binding: driver.bind_runtime(&refreshed_provider, &refreshed_credential)?,
            credential: Some(refreshed_credential),
        })
    }

    pub(super) async fn sync_provider_runtime_fields(
        &self,
        provider: &Provider,
        credential: &StoredCredential,
    ) -> anyhow::Result<Provider> {
        let Some(driver) = auth::build_driver(&credential.driver_key) else {
            return Ok(provider.clone());
        };

        let binding = driver.bind_runtime(provider, credential)?;
        let base_url = binding
            .base_url_override
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| provider.base_url.clone());
        let models_source = binding
            .models_source_override
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| provider.models_source.clone());

        self.gw
            .storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    base_url: Some(base_url),
                    models_source,
                    api_key: Some(String::new()),
                    auth_mode: Some("oauth".to_string()),
                    is_enabled: Some(provider.is_enabled),
                    ..Default::default()
                },
            )
            .await
    }

    pub async fn refresh_oauth_providers(&self) -> anyhow::Result<usize> {
        let oauth_store = self.gw.storage.oauth_credentials();

        // Recover stale refreshing credentials (timeout = 60s)
        let recovered = oauth_store
            .recover_stale_refreshing(std::time::Duration::from_secs(60))
            .await
            .unwrap_or(0);
        if recovered > 0 {
            tracing::info!("recovered {recovered} stale refreshing oauth credentials");
        }

        // Find credentials expiring within 300 seconds
        let expiring = oauth_store
            .list_expiring(std::time::Duration::from_secs(300))
            .await?;

        let mut refreshed = 0usize;
        for cred in expiring {
            let has_refresh = cred
                .refresh_token
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());
            if !has_refresh {
                continue;
            }

            let provider = match self.gw.storage.providers().get(&cred.provider_id).await? {
                Some(p) => p,
                None => continue,
            };

            match self.proactive_refresh_credential(&provider, &cred).await {
                Ok(_) => refreshed += 1,
                Err(error) => tracing::warn!(
                    "background oauth refresh failed for provider {} ({}): {}",
                    provider.id,
                    provider.name,
                    error
                ),
            }
        }

        Ok(refreshed)
    }

    /// All refresh entry points share validation, total network deadline,
    /// failure classification, and versioned publication. Runtime fields are
    /// changed only after this succeeds, never before the token CAS.
    async fn refresh_locked_credential(
        &self,
        provider: &Provider,
        credential: &StoredCredential,
        lease: &OAuthCredential,
        driver: &dyn auth::AuthDriver,
    ) -> anyhow::Result<CredentialBundle> {
        // Include error persistence in the hard 30-second budget.
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.refresh_locked_credential_inner(provider, credential, lease, driver),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("OAuth refresh deadline exceeded")))
    }

    async fn refresh_locked_credential_inner(
        &self,
        provider: &Provider,
        credential: &StoredCredential,
        lease: &OAuthCredential,
        driver: &dyn auth::AuthDriver,
    ) -> anyhow::Result<CredentialBundle> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
        let store = self.gw.storage.oauth_credentials();
        let operation = async {
            if credential
                .refresh_token
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                anyhow::bail!("provider oauth refresh token is missing; reauthentication required");
            }
            let client = if driver.metadata().key == "devin" {
                self.gw.devin_http_client(provider.use_proxy, false).await?
            } else {
                self.gw.http_client_for_provider(provider.use_proxy).await?
            };
            let mut bundle = driver
                .refresh(
                    credential,
                    RefreshAuthContext {
                        use_proxy: provider.use_proxy,
                        http_client: Some(client),
                        ..Default::default()
                    },
                )
                .await?;
            // Drivers may omit an unchanged refresh token, never erase it.
            if bundle
                .refresh_token
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                bundle.refresh_token = credential.refresh_token.clone();
            }
            if bundle
                .access_token
                .as_deref()
                .is_none_or(|token| token.trim().is_empty())
                || is_expired_at(bundle.expires_at.as_deref())
            {
                anyhow::bail!("OAuth refresh returned an invalid credential");
            }
            let candidate = stored_credential_from_bundle(
                driver.metadata().key,
                driver.metadata().scheme.as_str(),
                &bundle,
            );
            // Validate binding before persisting or making the new token visible.
            driver.bind_runtime(provider, &candidate)?;
            Ok(bundle)
        };
        let result = tokio::time::timeout_at(deadline, operation)
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("OAuth refresh deadline exceeded")));
        let bundle = match result {
            Ok(bundle) => bundle,
            Err(error) => {
                let (terminal, meta, message) = refresh_failure_policy(credential, &error);
                store
                    .fail_refresh_with_policy(
                        &provider.id,
                        lease.status_version,
                        &message,
                        terminal,
                        meta,
                    )
                    .await?;
                return Err(error);
            }
        };
        let input = upsert_credential_from_bundle(
            driver.metadata().key,
            driver.metadata().scheme.as_str(),
            &bundle,
        );
        match tokio::time::timeout_at(
            deadline,
            store.complete_refresh(&provider.id, lease.status_version, input),
        )
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "OAuth refresh publication deadline exceeded"
            ))
        }) {
            Ok(Some(_)) => Ok(bundle),
            Ok(None) => anyhow::bail!("OAuth refresh result was superseded by newer credentials"),
            Err(error) => Err(error),
        }
    }

    /// Proactively refresh an OAuth credential that is approaching expiry.
    /// Unlike `resolve_provider_runtime` (which skips refresh if the token is
    /// still valid), this always attempts to obtain a new token.
    async fn proactive_refresh_credential(
        &self,
        provider: &Provider,
        cred: &OAuthCredential,
    ) -> anyhow::Result<()> {
        let driver_key = if cred.driver_key.is_empty() {
            provider
                .vendor
                .as_deref()
                .map(auth::normalize_driver_key)
                .unwrap_or_default()
        } else {
            cred.driver_key.clone()
        };

        let credential = stored_credential_from_oauth(cred, &driver_key);

        let Some(driver) = auth::build_driver(&driver_key) else {
            anyhow::bail!("no auth driver found for key: {driver_key}");
        };

        // CAS lock: transition connected → refreshing
        let oauth_store = self.gw.storage.oauth_credentials();
        let locked = oauth_store
            .try_begin_refresh(&provider.id, cred.status_version)
            .await?;
        let Some(refresh_lock) = locked else {
            // Another caller is already refreshing — skip
            return Ok(());
        };

        let bundle = self
            .refresh_locked_credential(provider, &credential, &refresh_lock, driver.as_ref())
            .await?;
        let refreshed_credential =
            stored_credential_from_bundle(&driver_key, driver.metadata().scheme.as_str(), &bundle);
        self.sync_provider_runtime_fields(provider, &refreshed_credential)
            .await?;
        self.bump_config_epoch().await?;
        Ok(())
    }
}

fn refresh_failure_policy(
    _credential: &StoredCredential,
    error: &anyhow::Error,
) -> (bool, Option<String>, String) {
    (false, None, error.to_string())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    async fn gateway() -> anyhow::Result<Gateway> {
        let (gateway, _) = Gateway::new(crate::config::GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-oauth-lifecycle-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        })
        .await?;
        Ok(gateway)
    }
    #[tokio::test]
    async fn devin_manual_import_create_runtime_and_rebind_stay_native() -> anyhow::Result<()> {
        let gw = gateway().await?;
        let admin = gw.admin();
        let init = admin.init_oauth_session("devin", false).await?;
        assert!(init.requires_manual_code);
        assert_eq!(
            init.completion_mode,
            Some(auth::AuthCompletionMode::ManualExchange)
        );
        let ready = admin
            .complete_oauth_session(
                &init.session_id,
                auth::AuthExchangeInput {
                    code: Some("eyJlocal.fixture.signature".into()),
                    ..Default::default()
                },
            )
            .await?;
        assert!(matches!(ready, AuthSessionStatusData::Ready { .. }));
        let input: CreateProvider = serde_json::from_value(serde_json::json!({
            "name": "Devin local auth fixture", "protocol": "openai-compatible",
            "base_url": "https://unused.invalid", "api_key": ""
        }))?;
        let provider = admin
            .create_provider_with_oauth_session(&init.session_id, input)
            .await?;
        assert_eq!(provider.vendor.as_deref(), Some("devin"));
        assert_eq!(provider.channel.as_deref(), Some("cli"));
        assert!(matches!(
            provider.protocol.as_str(),
            "devin-chat" | "devin-chat/get-chat-message/v1"
        ));
        assert_eq!(provider.base_url, "https://server.codeium.com");
        assert_eq!(provider.protocol_mode, "fixed");
        assert!(provider.keys.is_empty());
        let status = admin.get_provider_oauth_status(&provider.id).await?;
        assert_eq!(status.status, "connected");
        assert!(!status.has_refresh_token);
        assert!(status.expires_at.is_none());
        let runtime = admin.resolve_provider_runtime(&provider).await?;
        assert_eq!(
            runtime.access_token,
            "devin-session-token$eyJlocal.fixture.signature"
        );
        assert!(runtime.binding.disable_default_auth);
        assert!(runtime.binding.extra_headers.is_empty());

        let reauth = admin.init_oauth_session("devin", false).await?;
        admin
            .complete_oauth_session(
                &reauth.session_id,
                auth::AuthExchangeInput {
                    code: Some("devin-session-token$replacement-fixture".into()),
                    ..Default::default()
                },
            )
            .await?;
        let session = admin
            .get_auth_session_record(&reauth.session_id)
            .await?
            .unwrap();
        let mut incompatible = provider.clone();
        incompatible.protocol = "openai-compatible".into();
        assert!(validate_oauth_session_for_provider(&session, &incompatible).is_err());
        incompatible = provider.clone();
        incompatible.base_url = "https://unused.invalid".into();
        assert!(validate_oauth_session_for_provider(&session, &incompatible).is_err());
        let rebound = admin
            .bind_provider_with_oauth_session(&provider.id, &reauth.session_id)
            .await?;
        assert_eq!(
            admin.resolve_provider_runtime(&rebound).await?.access_token,
            "devin-session-token$replacement-fixture"
        );
        admin.logout_provider_oauth(&provider.id).await?;
        assert!(admin.resolve_provider_runtime(&provider).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn session_flight_excludes_overlap_and_cancel_rejects_late_publish() -> anyhow::Result<()>
    {
        let gw = gateway().await?;
        let admin = gw.admin();
        let init = admin.init_oauth_session("codex", false).await?;
        gw.auth_session_controls
            .lock()
            .await
            .get_mut(&init.session_id)
            .unwrap()
            .next_poll_at = None;
        let flight = admin
            .begin_session_flight(&init.session_id, true)
            .await?
            .unwrap();
        assert!(
            admin
                .begin_session_flight(&init.session_id, true)
                .await?
                .is_none()
        );
        admin.cancel_oauth_session(&init.session_id).await?;
        assert!(flight.cancellation.is_cancelled());
        assert!(
            admin
                .publish_session_progress(
                    &init.session_id,
                    &flight,
                    UpdateAuthSession {
                        status: Some("ready".into()),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
        );
        assert!(
            admin
                .get_auth_session_record(&init.session_id)
                .await?
                .is_none()
        );
        Ok(())
    }
    #[tokio::test]
    async fn dropped_flight_releases_busy_and_ready_ttl_is_not_token_ttl() -> anyhow::Result<()> {
        let gw = gateway().await?;
        let admin = gw.admin();
        let init = admin.init_oauth_session("codex", false).await?;
        gw.auth_session_controls
            .lock()
            .await
            .get_mut(&init.session_id)
            .unwrap()
            .next_poll_at = None;
        let old = admin
            .begin_session_flight(&init.session_id, true)
            .await?
            .unwrap();
        let old_generation = old.generation;
        drop(old);
        let next = admin
            .begin_session_flight(&init.session_id, true)
            .await?
            .unwrap();
        assert!(next.generation > old_generation);
        let bundle = CredentialBundle {
            access_token: Some("test".into()),
            expires_at: Some("2099-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        let session = admin
            .publish_session_progress(
                &init.session_id,
                &next,
                UpdateAuthSession {
                    status: Some("ready".into()),
                    result_json: Some(serde_json::to_string(&bundle)?),
                    expires_at: Some(auth::drivers::expires_at_after(120)),
                    ..Default::default()
                },
            )
            .await?;
        assert!((115..=120).contains(&remaining_seconds_until(session.expires_at.as_deref())));
        assert!(matches!(
            build_auth_session_ready_data(&session, &bundle),
            AuthSessionStatusData::Ready {
                expires_in: 115..=120,
                ..
            }
        ));
        Ok(())
    }
}
