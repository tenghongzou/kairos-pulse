//! 版本協商（商業審計 A3）：hello 的 `proto`／`proto_min`／`proto_max`／`features` 與
//! hello_ack 的 `proto`／`features`。規則見 PROTOCOL.md「版本協商」。

use crate::{
    ErrorCode, FEATURE_INVENTORY, FEATURE_PROCESSES, PROTO_V1, PROTO_V2, Proto, ProtoOffer,
    ServerMsg,
};

/// 協定版本範圍（含兩端）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtoRange {
    pub min: u32,
    pub max: u32,
}

/// 本 crate（新版 Kairos 與 Pulse）支援的版本範圍
pub const SUPPORTED_PROTO: ProtoRange = ProtoRange {
    min: PROTO_V1,
    max: PROTO_V2,
};

/// report 改用通用指標 `metrics`（A4）。雙方都宣告時才使用，否則 report 沿用 v1 的三個欄位
pub const FEATURE_METRICS: &str = "metrics";

/// report 改為 at-least-once（A5）：report 帶 `boot_id`／`report_seq`，伺服器據此去重，
/// client 在收到 `ack` 前保留資料、斷線或結果不明時重送。雙方都宣告時才啟用，
/// 否則維持 at-most-once（見 PROTOCOL.md「durable」）
pub const FEATURE_DURABLE: &str = "durable";

/// Kairos 在 v2 hello_ack 宣告的功能（Pulse 啟用本機緩衝時也宣告同一組，停用時不宣告 durable）
pub const SERVER_FEATURES: &[&str] = &[FEATURE_METRICS, FEATURE_DURABLE];

/// [`client_proto_range`] 的失敗原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoRangeError {
    /// `proto` 不是合法 u32：回 `unsupported_version`（與 v1 相同）
    InvalidProto,
    /// `proto_min`／`proto_max`／`features` 型別錯誤或範圍不一致：回 `bad_request`（帶 hello 的 seq）
    BadOffer(String),
}

/// hello 宣告的版本範圍。
///
/// - `proto` 不是合法 u32 → [`ProtoRangeError::InvalidProto`]
/// - 沒有 `proto_min` 與 `proto_max` → `[proto, proto]`（v1 client）
/// - 只有其中一個、`proto_min > proto_max`、或 `proto` 不在範圍內 → [`ProtoRangeError::BadOffer`]
pub fn client_proto_range(
    proto: &Proto,
    offer: &Result<ProtoOffer, String>,
) -> Result<ProtoRange, ProtoRangeError> {
    let Proto::Version(proto) = *proto else {
        return Err(ProtoRangeError::InvalidProto);
    };
    let offer = offer.as_ref().map_err(|e| {
        ProtoRangeError::BadOffer(format!("invalid proto_min, proto_max or features: {e}"))
    })?;
    match (offer.proto_min, offer.proto_max) {
        (None, None) => Ok(ProtoRange {
            min: proto,
            max: proto,
        }),
        (Some(min), Some(max)) => {
            if min > max {
                Err(ProtoRangeError::BadOffer(format!(
                    "proto_min ({min}) greater than proto_max ({max})"
                )))
            } else if !(min..=max).contains(&proto) {
                Err(ProtoRangeError::BadOffer(format!(
                    "proto ({proto}) must be within proto_min..=proto_max ({min}..={max})"
                )))
            } else {
                Ok(ProtoRange { min, max })
            }
        }
        _ => Err(ProtoRangeError::BadOffer(
            "proto_min and proto_max must be given together".to_string(),
        )),
    }
}

/// 協商結果：雙方範圍重疊時取最高的共同版本，沒有重疊時 `None`（回 `unsupported_version`）
pub fn negotiate(client: ProtoRange, server: ProtoRange) -> Option<u32> {
    let version = client.max.min(server.max);
    (version >= client.min.max(server.min)).then_some(version)
}

/// `unsupported_version` 錯誤：`message` 與 `proto_min`／`proto_max` 都帶伺服器支援的範圍
pub fn unsupported_version_error(seq: Option<u64>, server: ProtoRange) -> ServerMsg {
    let message = if server.min == server.max {
        format!("supported proto: {}", server.min)
    } else {
        format!("supported proto: {}-{}", server.min, server.max)
    };
    match ServerMsg::error(seq, ErrorCode::UnsupportedVersion, message) {
        ServerMsg::Error {
            seq,
            code,
            message,
            fatal,
            ..
        } => ServerMsg::Error {
            seq,
            code,
            message,
            fatal,
            proto_min: Some(server.min),
            proto_max: Some(server.max),
            retry_after_ms: None,
        },
        other => other,
    }
}

/// 一條連線協商後的狀態（兩端各自依 hello／hello_ack 算出，結果相同）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// 協商出的版本
    pub proto: u32,
    /// report 是否使用通用指標 `metrics`（v2 且雙方都宣告 [`FEATURE_METRICS`]）
    pub metrics: bool,
    /// report 是否為 at-least-once（v2 且雙方都宣告 [`FEATURE_DURABLE`]）：report 必須帶
    /// `boot_id`／`report_seq`，伺服器去重
    pub durable: bool,
    /// 是否送 `inventory` 訊息（v2 且雙方都宣告 [`FEATURE_INVENTORY`]；METRICS.md）
    pub inventory: bool,
    /// report 是否另帶 `processes`（v2、協商了 `metrics`，且雙方都宣告 [`FEATURE_PROCESSES`]；METRICS.md）
    pub processes: bool,
}

impl Session {
    /// v1 連線：report 為三個欄位，hello_ack 不帶 v2 欄位
    pub const V1: Session = Session {
        proto: PROTO_V1,
        metrics: false,
        durable: false,
        inventory: false,
        processes: false,
    };

    /// 依協商出的版本與雙方宣告的功能建立。v1 時忽略 features。
    pub fn new<A: AsRef<str>, B: AsRef<str>>(proto: u32, ours: &[A], theirs: &[B]) -> Self {
        let both = |feature: &str| {
            ours.iter().any(|f| f.as_ref() == feature)
                && theirs.iter().any(|f| f.as_ref() == feature)
        };
        let v2 = proto >= PROTO_V2;
        let metrics = v2 && both(FEATURE_METRICS);
        Session {
            proto,
            metrics,
            durable: v2 && both(FEATURE_DURABLE),
            inventory: v2 && both(FEATURE_INVENTORY),
            processes: metrics && both(FEATURE_PROCESSES),
        }
    }

    /// hello_ack 等訊息是否帶 v2 欄位
    pub fn is_v2(&self) -> bool {
        self.proto >= PROTO_V2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(min: Option<u32>, max: Option<u32>) -> Result<ProtoOffer, String> {
        Ok(ProtoOffer {
            proto_min: min,
            proto_max: max,
            features: None,
        })
    }

    fn range(min: u32, max: u32) -> ProtoRange {
        ProtoRange { min, max }
    }

    #[test]
    fn v1_client_range_is_its_proto() {
        let v1 = Ok(ProtoOffer::default());
        assert_eq!(client_proto_range(&Proto::Version(1), &v1), Ok(range(1, 1)));
        assert_eq!(client_proto_range(&Proto::Version(7), &v1), Ok(range(7, 7)));
        assert_eq!(
            client_proto_range(&Proto::Invalid("\"1\"".into()), &v1),
            Err(ProtoRangeError::InvalidProto)
        );
        // proto 不合法時優先回 unsupported_version（與 v1 相同），即使協商欄位也壞了
        assert_eq!(
            client_proto_range(&Proto::Invalid("-1".into()), &Err("x".into())),
            Err(ProtoRangeError::InvalidProto)
        );
    }

    #[test]
    fn v2_client_range_must_be_consistent() {
        let p1 = Proto::Version(1);
        assert_eq!(
            client_proto_range(&p1, &offer(Some(1), Some(2))),
            Ok(range(1, 2))
        );
        assert_eq!(
            client_proto_range(&p1, &offer(Some(1), Some(1))),
            Ok(range(1, 1))
        );
        for bad in [
            offer(Some(1), None),
            offer(None, Some(2)),
            offer(Some(2), Some(1)),
            offer(Some(2), Some(3)), // proto 不在範圍內
            Err("invalid type".to_string()),
        ] {
            assert!(
                matches!(
                    client_proto_range(&p1, &bad),
                    Err(ProtoRangeError::BadOffer(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn negotiation_picks_the_highest_common_version() {
        let server = SUPPORTED_PROTO;
        assert_eq!(negotiate(range(1, 1), server), Some(1)); // 舊 Pulse ↔ 新 Kairos
        assert_eq!(negotiate(range(1, 2), server), Some(2)); // 新 ↔ 新
        assert_eq!(negotiate(range(1, 2), range(1, 1)), Some(1)); // 新 Pulse ↔ 只支援 v1 的伺服器
        assert_eq!(negotiate(range(2, 2), server), Some(2));
        assert_eq!(negotiate(range(1, 9), server), Some(2)); // 未來的 Pulse
        assert_eq!(negotiate(range(2, 9), server), Some(2));
        assert_eq!(negotiate(range(0, 0), server), None);
        assert_eq!(negotiate(range(3, 4), server), None);
        assert_eq!(negotiate(range(2, 2), range(1, 1)), None);
        assert_eq!(negotiate(range(u32::MAX, u32::MAX), server), None);
    }

    #[test]
    fn unsupported_version_error_carries_the_server_range() {
        let v = serde_json::to_value(unsupported_version_error(Some(1), SUPPORTED_PROTO)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type":"error","seq":1,"code":"unsupported_version",
                "message":"supported proto: 1-2","fatal":true,"proto_min":1,"proto_max":2})
        );
        let v = serde_json::to_value(unsupported_version_error(None, range(1, 1))).unwrap();
        assert_eq!(v["message"], "supported proto: 1");
        assert_eq!(v["seq"], serde_json::Value::Null);
    }

    #[test]
    fn metrics_need_v2_and_both_sides() {
        let both = [FEATURE_METRICS];
        let none: [&str; 0] = [];
        assert_eq!(
            Session::new(2, &both, &["x", FEATURE_METRICS]),
            Session {
                proto: 2,
                metrics: true,
                durable: false,
                inventory: false,
                processes: false,
            }
        );
        assert!(!Session::new(2, &both, &none).metrics);
        assert!(!Session::new(2, &none, &both).metrics);
        assert!(!Session::new(1, &both, &both).metrics);
        assert_eq!(Session::new(1, &both, &both), Session::V1);
        assert!(Session::new(2, &none, &none).is_v2());
        assert!(!Session::V1.is_v2());
    }

    #[test]
    fn durable_needs_v2_and_both_sides() {
        // 相容矩陣：任一端是舊版（不宣告 durable）或協商為 v1 → 維持 at-most-once
        let new = [FEATURE_METRICS, FEATURE_DURABLE];
        let old = [FEATURE_METRICS];
        let none: [&str; 0] = [];
        let both = Session::new(2, &new, &new);
        assert!(both.durable && both.metrics);
        assert!(!Session::new(2, &new, &old).durable); // 0.2.0 Pulse
        assert!(!Session::new(2, &old, &new).durable); // 0.2.0 Kairos
        assert!(!Session::new(2, &new, &none).durable);
        assert!(!Session::new(1, &new, &new).durable); // 協商為 v1
        // durable 與 metrics 各自協商
        let only_durable = Session::new(2, &new, &[FEATURE_DURABLE]);
        assert!(only_durable.durable && !only_durable.metrics);
        assert_eq!(Session::new(1, &new, &new), Session::V1);
    }

    #[test]
    fn inventory_and_processes_need_v2_and_both_sides() {
        // 相容矩陣（METRICS.md「相容性」）：0.7 以前的 Kairos／Pulse 不宣告這兩個 feature
        let new = [
            FEATURE_METRICS,
            FEATURE_DURABLE,
            FEATURE_INVENTORY,
            FEATURE_PROCESSES,
        ];
        let old = [FEATURE_METRICS, FEATURE_DURABLE];
        let s = Session::new(2, &new, &new);
        assert!(s.inventory && s.processes && s.metrics && s.durable);
        for (ours, theirs) in [(&new[..], &old[..]), (&old[..], &new[..])] {
            let s = Session::new(2, ours, theirs);
            assert!(!s.inventory && !s.processes && s.metrics && s.durable);
        }
        assert_eq!(Session::new(1, &new, &new), Session::V1);
        // inventory 不依賴 metrics；processes 是 report 的欄位，必須協商了 metrics
        let no_metrics = [FEATURE_INVENTORY, FEATURE_PROCESSES];
        let s = Session::new(2, &no_metrics, &new);
        assert!(s.inventory && !s.processes && !s.metrics);
        // 新功能不在 SERVER_FEATURES：由 Kairos 實作時依儲存後端決定是否宣告（METRICS.md）
        assert!(!SERVER_FEATURES.contains(&FEATURE_INVENTORY));
        assert!(!SERVER_FEATURES.contains(&FEATURE_PROCESSES));
    }
}
