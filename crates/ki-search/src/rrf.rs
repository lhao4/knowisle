//! Reciprocal Rank Fusion：把多路候选合成一条有序列表。
//!
//! # 为什么是 RRF 而不是加权分数
//!
//! 各路返回的分数**不同量纲**：`bm25()` 是负的、越小越相关；文件名通道是
//! `LIKE` 命中，没有分数可言；将来若加 `trigram` 通道，它的分数又是另一套。
//! 归一化它们需要知道每路的分布，而分布随查询变。RRF 只用**名次**，
//! 天生免疫量纲问题——这也是它在多路检索里成为默认做法的原因。
//!
//! `score(d) = Σ 1 / (k + rank_i(d))`，`k = 60` 是论文里的经验值：
//! 它把「第 1 名与第 2 名」的差距压到 1/61 vs 1/62，于是**多路都上榜**
//! 比**单路第一**更值钱。k 越小越看重头部，越大越平均。
//!
//! # 现在只有一两路，为什么还留着
//!
//! 每次查询只跑 A 或 B 之一（[`crate::plan`] 决定），再加文件名通道。
//! 所以今天它退化成「拼接去重」。留着是因为：
//!
//! 1. 文件名通道的候选必须**全部进池**（`name_hit` 是旧契约的第一排序键，
//!    不能被截断挤掉），这已经是一次真正的合并；
//! 2. ADR-007 里写明的偿还计划是「加 trigram 第三通道」，那时它立刻变成必需品；
//! 3. 删掉它省不下 30 行，但将来加回来要重新想一遍 tie-break。
//!
//! # 不做通道加权
//!
//! A/B 互斥，权重无意义；N 的重要性由上层排序的第一键 `name_hit` 保证，
//! 加权只增加解释成本。等真的有三路再说。

use std::collections::HashMap;

/// RRF 的平滑常数。论文经验值，改动必须同步 ADR-007。
pub const RRF_K: f64 = 60.0;

/// 融合后的一条候选。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fused {
    pub id: i64,
    pub score: f64,
}

/// 融合多路有序候选。
///
/// `channels` 里每个元素是一路的名次列表（下标 0 是第一名）。
/// 返回按 `score` 降序、同分按 `id` 升序——**完全确定**，与 `HashMap`
/// 的遍历顺序无关。
///
/// 注意这里不产生最终顺序：最终排序的四个键（`name_hit` → `score` →
/// `path` → `line`）需要路径与行号，只有上层拿得到。这个函数只保证
/// 「谁进池、按分数排」这一层是确定的。
pub fn fuse(channels: &[Vec<i64>]) -> Vec<Fused> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for channel in channels {
        for (rank, id) in channel.iter().enumerate() {
            *scores.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }

    let mut fused: Vec<Fused> = scores
        .into_iter()
        .map(|(id, score)| Fused { id, score })
        .collect();
    // f64 没有 Ord，`partial_cmp` 遇到 NaN 会返回 None 并让 sort_by 的
    // 比较函数不满足全序（Rust 1.81+ 会直接 panic）。`total_cmp` 是全序，
    // 而这里的分母恒 ≥ 61，本来也不会出 NaN——用 total_cmp 是为了让
    // 「不会 NaN」这件事不依赖调用方的输入。
    fused.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_channel_keeps_rank_order() {
        let fused = fuse(&[vec![3, 1, 2]]);
        assert_eq!(fused.iter().map(|f| f.id).collect::<Vec<_>>(), [3, 1, 2]);
    }

    /// 两路都上榜的候选必须排在只有一路的候选前面——
    /// 这就是 RRF 的全部价值，也是它值得留下的理由。
    #[test]
    fn agreement_beats_a_single_first_place() {
        // 7 在两路都是第 2；9 只在第一路是第 1
        let fused = fuse(&[vec![9, 7], vec![5, 7]]);
        assert_eq!(fused[0].id, 7);
    }

    #[test]
    fn duplicates_across_channels_are_merged() {
        let fused = fuse(&[vec![1, 2], vec![1, 2]]);
        assert_eq!(fused.len(), 2);
        assert!(fused[0].score > 0.0);
    }

    /// 同分时按 id 升序，保证与 HashMap 的遍历顺序无关。
    #[test]
    fn ties_are_broken_deterministically() {
        let a = fuse(&[vec![5, 3, 8, 1]]);
        let b = fuse(&[vec![5, 3, 8, 1]]);
        assert_eq!(a, b);

        // 构造真正同分的两路：每路各给一个第一名
        let fused = fuse(&[vec![20], vec![10]]);
        assert_eq!(fused.iter().map(|f| f.id).collect::<Vec<_>>(), [10, 20]);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(fuse(&[]).is_empty());
        assert!(fuse(&[vec![], vec![]]).is_empty());
    }

    /// 分数必须随名次单调下降。
    #[test]
    fn score_decreases_with_rank() {
        let fused = fuse(&[vec![1, 2, 3, 4]]);
        for pair in fused.windows(2) {
            assert!(pair[0].score > pair[1].score);
        }
        assert!((fused[0].score - 1.0 / (RRF_K + 1.0)).abs() < 1e-12);
    }
}
