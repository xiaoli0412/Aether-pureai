use crate::handlers::admin::request::AdminAppState;
use crate::handlers::admin::system::shared::modules as admin_system_modules;
use crate::handlers::shared::module_available_from_env;
use crate::GatewayError;
use axum::{body::Bytes, http};
use serde_json::json;

impl<'a> AdminAppState<'a> {
    pub(crate) async fn build_admin_modules_status_payload(
        &self,
    ) -> Result<serde_json::Value, GatewayError> {
        admin_system_modules::build_admin_modules_status_payload(self).await
    }

    pub(crate) async fn build_admin_module_status_payload(
        &self,
        module_name: &str,
    ) -> Result<Result<serde_json::Value, (http::StatusCode, serde_json::Value)>, GatewayError>
    {
        let Some(module) = admin_system_modules::admin_module_by_name(module_name) else {
            return Ok(Err((
                http::StatusCode::NOT_FOUND,
                json!({ "detail": format!("模块 '{module_name}' 不存在") }),
            )));
        };
        let runtime = admin_system_modules::build_admin_module_runtime_state(self).await?;
        Ok(Ok(admin_system_modules::build_admin_module_status_payload(
            self, module, &runtime,
        )
        .await?))
    }

    pub(crate) async fn set_admin_module_enabled_payload(
        &self,
        module_name: &str,
        request_body: &Bytes,
    ) -> Result<Result<serde_json::Value, (http::StatusCode, serde_json::Value)>, GatewayError>
    {
        let Some(module) = admin_system_modules::admin_module_by_name(module_name) else {
            return Ok(Err((
                http::StatusCode::NOT_FOUND,
                json!({ "detail": format!("模块 '{module_name}' 不存在") }),
            )));
        };
        let available = module_available_from_env(module.env_key, module.default_available);
        if !available {
            return Ok(Err((
                http::StatusCode::BAD_REQUEST,
                json!({
                    "detail": format!(
                        "模块 '{}' 不可用，无法启用。请检查环境变量 {} 和依赖库。",
                        module.name, module.env_key
                    )
                }),
            )));
        }
        let payload = match serde_json::from_slice::<
            admin_system_modules::AdminSetModuleEnabledRequest,
        >(request_body)
        {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(Err((
                    http::StatusCode::BAD_REQUEST,
                    json!({ "detail": "请求体格式错误，需要 enabled 字段" }),
                )));
            }
        };

        let runtime = admin_system_modules::build_admin_module_runtime_state(self).await?;
        if payload.enabled {
            let (config_validated, config_error) =
                admin_system_modules::build_admin_module_validation_result(module, &runtime);
            if !config_validated {
                return Ok(Err((
                    http::StatusCode::BAD_REQUEST,
                    json!({
                        "detail": format!(
                            "模块配置未验证通过: {}",
                            config_error.unwrap_or_else(|| "未知错误".to_string())
                        )
                    }),
                )));
            }
        }

        let enabled_config_key = admin_system_modules::admin_module_enabled_config_key(module);
        if module.name == "empty_response_shield" || module.name == "cost_tier_routing" {
            // These modules keep their enabled flag inside the runtime config
            // object; merge instead of overwriting the rest of the settings.
            let current = self
                .read_system_config_json_value(&enabled_config_key)
                .await?;
            let mut config_object = current
                .and_then(|value| value.as_object().map(ToOwned::to_owned))
                .unwrap_or_default();
            config_object.insert("enabled".to_string(), json!(payload.enabled));
            let _ = self
                .upsert_system_config_json_value(
                    &enabled_config_key,
                    &serde_json::Value::Object(config_object),
                    Some(&format!("模块 [{}] 启用状态", module.display_name)),
                )
                .await?;
        } else {
            let _ = self
                .upsert_system_config_json_value(
                    &enabled_config_key,
                    &json!(payload.enabled),
                    Some(&format!("模块 [{}] 启用状态", module.display_name)),
                )
                .await?;
        }
        let updated_runtime = admin_system_modules::build_admin_module_runtime_state(self).await?;
        Ok(Ok(admin_system_modules::build_admin_module_status_payload(
            self,
            module,
            &updated_runtime,
        )
        .await?))
    }
}
