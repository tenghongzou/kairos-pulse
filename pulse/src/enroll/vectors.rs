//! 測試用：讀入 `crates/kairos-proto/ENROLL.md` 的「測試向量」節（兩端共用的金鑰、CSR、簽章內容與
//! 雜湊）。Pulse 端的單元測試以它確認自己組出的請求與 `kairos-proto` 及 Kairos 端一致。

use std::collections::HashMap;

const ENROLL_MD: &str = include_str!("../../../crates/kairos-proto/ENROLL.md");

pub struct Vectors {
    values: HashMap<String, String>,
    blocks: HashMap<String, String>,
}

impl Vectors {
    pub fn load() -> Self {
        let section = ENROLL_MD
            .split("\n## 測試向量")
            .nth(1)
            .expect("ENROLL.md 應有「測試向量」節");
        let mut values = HashMap::new();
        let mut blocks = HashMap::new();
        let mut heading = String::new();
        let mut in_block = false;
        let mut block = Vec::new();
        for line in section.lines() {
            if let Some(h) = line.strip_prefix("### ") {
                heading = h.trim().to_string();
                continue;
            }
            if line.starts_with("```") {
                if in_block {
                    blocks.insert(heading.clone(), block.join("\n"));
                    block.clear();
                }
                in_block = !in_block;
                continue;
            }
            if in_block {
                block.push(line.to_string());
                if let Some((k, v)) = line.split_once(" = ") {
                    values.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        Self { values, blocks }
    }

    /// `key = value` 的值
    pub fn get(&self, key: &str) -> &str {
        self.values
            .get(key)
            .unwrap_or_else(|| panic!("測試向量缺少 {key}"))
    }

    /// 某個標題下的 text 區塊（簽章內容）
    pub fn block(&self, heading: &str) -> &str {
        self.blocks
            .get(heading)
            .unwrap_or_else(|| panic!("測試向量缺少「{heading}」區塊"))
    }
}
