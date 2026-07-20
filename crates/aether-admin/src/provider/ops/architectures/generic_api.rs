use super::{
    json_object, ProviderOpsActionSpec, ProviderOpsArchitectureSpec, ProviderOpsAuthSpec,
    ProviderOpsBalanceMode, ProviderOpsCheckinMode, ProviderOpsVerifyMode,
};
use serde_json::{json, Map, Value};

pub(super) fn spec() -> ProviderOpsArchitectureSpec {
    let credentials_schema = json!({
        "type": "object",
        "properties": {
            "api_key": {
                "type": "string",
                "title": "API Key",
                "description": "提供商的 API Key",
                "x-sensitive": true,
                "x-input-type": "password"
            },
            "base_url": {
                "type": "string",
                "title": "站点地址",
                "description": "API 基础地址"
            }
        },
        "required": ["api_key"],
        "x-auth-method": "bearer",
        "x-auth-type": "api_key",
        "x-currency": "USD",
        "x-field-groups": [
            { "fields": ["base_url"] },
            { "fields": ["api_key"] }
        ],
        "x-quota-divisor": null,
        "x-validation": [
            {
                "type": "required",
                "fields": ["api_key"],
                "message": "请填写 API Key"
            }
        ]
    });

    ProviderOpsArchitectureSpec {
        architecture_id: "generic_api",
        display_name: "通用 API",
        description: "可配置的通用 API 架构，适用于各种中转站",
        hidden: true,
        credentials_schema: credentials_schema.clone(),
        verify_endpoint: "/api/user/self",
        verify_mode: ProviderOpsVerifyMode::DirectGet,
        balance_mode: ProviderOpsBalanceMode::SingleRequest,
        checkin_mode: ProviderOpsCheckinMode::NewApiCompatible,
        query_balance_cookie_auth_errors: false,
        supported_auth_types: vec![ProviderOpsAuthSpec {
            auth_type: "api_key",
            display_name: "API Key",
            credentials_schema,
        }],
        supported_actions: vec![ProviderOpsActionSpec {
            action_type: "query_balance",
            display_name: "查询余额",
            description: "查询 New API 账户余额信息",
            config_schema: json!({
                "type": "object",
                "properties": {
                    "endpoint": {
                        "type": "string",
                        "title": "API 路径",
                        "description": "余额查询 API 路径",
                        "default": "/api/user/self"
                    },
                    "method": {
                        "type": "string",
                        "title": "请求方法",
                        "enum": ["GET", "POST"],
                        "default": "GET"
                    },
                    "quota_divisor": {
                        "type": "number",
                        "title": "额度除数",
                        "description": "将原始额度值转换为美元的显式除数"
                    },
                    "currency": {
                        "type": "string",
                        "title": "货币单位",
                        "default": "USD"
                    }
                },
                "required": ["quota_divisor"]
            }),
        }],
        default_connector: Some("api_key"),
    }
}

pub(super) fn default_action_config(action_type: &str) -> Option<Map<String, Value>> {
    match action_type {
        "query_balance" => Some(json_object(json!({
            "endpoint": "/api/user/balance",
            "method": "GET"
        }))),
        "checkin" => Some(json_object(json!({
            "endpoint": "/api/user/checkin",
            "method": "POST"
        }))),
        _ => None,
    }
}
