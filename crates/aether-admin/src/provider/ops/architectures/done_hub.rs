use super::{
    json_object, ProviderOpsActionSpec, ProviderOpsArchitectureSpec, ProviderOpsAuthSpec,
    ProviderOpsBalanceMode, ProviderOpsCheckinMode, ProviderOpsVerifyMode,
};
use serde_json::{json, Map, Value};

pub(super) fn spec() -> ProviderOpsArchitectureSpec {
    let credentials_schema = json!({
        "type": "object",
        "properties": {
            "base_url": {
                "type": "string",
                "title": "站点地址",
                "description": "API 基础地址"
            },
            "session_cookie": {
                "type": "string",
                "title": "Session Cookie",
                "description": "从浏览器复制的 session Cookie 值",
                "x-sensitive": true,
                "x-input-type": "password"
            }
        },
        "required": ["session_cookie"],
        "x-auth-type": "cookie",
        "x-currency": "USD",
        "x-field-groups": [
            { "fields": ["base_url"] },
            { "fields": ["session_cookie"] }
        ],
        "x-quota-divisor": null,
        "x-validation": [
            {
                "type": "required",
                "fields": ["session_cookie"],
                "message": "请填写 Session Cookie"
            }
        ]
    });

    ProviderOpsArchitectureSpec {
        architecture_id: "done_hub",
        display_name: "Done-hub",
        description: "Done-hub 中转站预设配置，使用 Cookie 认证",
        hidden: false,
        credentials_schema: credentials_schema.clone(),
        verify_endpoint: "/api/user/profile",
        verify_mode: ProviderOpsVerifyMode::DirectGet,
        balance_mode: ProviderOpsBalanceMode::SingleRequest,
        checkin_mode: ProviderOpsCheckinMode::None,
        query_balance_cookie_auth_errors: true,
        supported_auth_types: vec![ProviderOpsAuthSpec {
            auth_type: "cookie",
            display_name: "Done-hub Cookie",
            credentials_schema,
        }],
        supported_actions: vec![ProviderOpsActionSpec {
            action_type: "query_balance",
            display_name: "查询余额",
            description: "查询 Done-hub 账户余额信息",
            config_schema: json!({
                "type": "object",
                "properties": {
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
        default_connector: Some("cookie"),
    }
}

pub(super) fn default_action_config(action_type: &str) -> Option<Map<String, Value>> {
    match action_type {
        "query_balance" => Some(json_object(json!({
            "endpoint": "/api/user/profile",
            "method": "GET",
            "currency": "USD"
        }))),
        _ => None,
    }
}
