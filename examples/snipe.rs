//! Snipe example for polyfill-rs
//!
//! 这个示例演示了高频交易技术，包括：
//! - 实时订单簿监控
//! - 过时报价检测
//! - 快速订单执行
//! - 市场影响分析
//!
//! ## 什么是 "Snipe" 策略？
//! "Snipe"（狙击）是一种高频交易策略，其目标是快速识别并捕捉市场中出现的短暂交易机会。
//! 当订单簿中的买卖价差（spread）缩小到特定阈值以下时，策略会立即执行交易来捕捉这个价差。
//!
//! ## 如何获取真实市场数据？
//! 目前这个示例使用的是 MockMarketData 来生成模拟数据。要连接真实的市场数据流，你需要：
//!
//! ```rust
//! use polyfill_rs::{WebSocketStream, StreamMessage};
//!
//! // 1. 创建 WebSocket 连接
//! let mut stream = WebSocketStream::new("wss://clob.polymarket.com/ws");
//!
//! // 2. 设置认证（如果需要）
//! let auth = WssAuth {
//!     address: "your_eth_address".to_string(),
//!     signature: "your_signature".to_string(),
//!     timestamp: chrono::Utc::now().timestamp() as u64,
//!     nonce: "random_nonce".to_string(),
//! };
//! stream = stream.with_auth(auth);
//!
//! // 3. 订阅市场数据频道
//! stream.subscribe_market_channel(vec!["token_id_1".to_string()]).await?;
//!
//! // 4. 在循环中处理实时消息
//! while let Some(message) = stream.next().await {
//!     match message? {
//!         StreamMessage::MarketBookUpdate { data } => {
//!             strategy.process_update(StreamMessage::BookUpdate { data })?;
//!         },
//!         StreamMessage::MarketTrade { data } => {
//!             strategy.process_update(StreamMessage::Trade { data })?;
//!         },
//!         StreamMessage::Heartbeat { timestamp } => {
//!             strategy.process_update(StreamMessage::Heartbeat { timestamp })?;
//!         },
//!         _ => {}
//!     }
//! }
//! ```

use polyfill_rs::{
    book::OrderBookManager,
    errors::Result,
    fill::{FillEngine, FillStatus},
    types::*,
    utils::time,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// 狙击交易策略
/// 
/// 这个结构体包含了执行 "snipe" 策略所需的所有状态和配置。
/// 它监控订单簿的变化，检测交易机会，并执行快速订单。
#[derive(Debug)]
pub struct SnipeStrategy {
    /// 目标代币 ID（要交易的市场代币）
    token_id: String,

    /// 最大价差百分比（超过此值不交易）
    /// 例如：2.0 表示只交易价差 <= 2% 的机会
    max_spread_pct: Decimal,
    
    /// 最小订单大小（每次交易的最小金额）
    min_order_size: Decimal,

    /// 最大订单大小（每次交易的最大金额）
    max_order_size: Decimal,
    
    /// 过时报价阈值（秒）
    /// 如果订单簿数据超过此时间未更新，认为数据已过时
    stale_threshold: u64,
    
    /// 上一次记录的最佳买价（best bid）
    last_best_bid: Option<Decimal>,
    
    /// 上一次记录的最佳卖价（best ask）
    last_best_ask: Option<Decimal>,
    
    /// 最后一次更新的时间戳（秒）
    last_update: u64,
    
    /// 订单簿管理器：维护本地订单簿状态
    /// 用于快速查询买卖价格和流动性
    book_manager: OrderBookManager,
    
    /// 订单执行引擎：用于模拟和执行市价单
    /// 计算平均成交价、滑点、费用等
    fill_engine: FillEngine,
    
    /// 统计数据：记录策略的运行统计信息
    stats: SnipeStats,
}

/// 狙击策略的统计数据
/// 
/// 记录策略运行过程中的关键指标，用于评估策略表现
#[derive(Debug, Clone)]
pub struct SnipeStats {
    /// 检测到的交易机会数量（价差满足条件的情况）
    pub opportunities_detected: u64,
    
    /// 已提交的订单数量
    pub orders_placed: u64,
    
    /// 已成交的订单数量
    pub orders_filled: u64,
    
    /// 总交易量（累计成交的代币数量）
    pub total_volume: Decimal,
    
    /// 总盈亏（Profit & Loss）
    pub total_pnl: Decimal,
    
    /// 平均成交时间（毫秒）
    /// 衡量订单执行速度，越小越好
    pub avg_fill_time_ms: f64,
}

impl Default for SnipeStats {
    fn default() -> Self {
        Self {
            opportunities_detected: 0,
            orders_placed: 0,
            orders_filled: 0,
            total_volume: dec!(0),
            total_pnl: dec!(0),
            avg_fill_time_ms: 0.0,
        }
    }
}

impl SnipeStrategy {
    /// 创建一个新的狙击策略实例
    /// 
    /// # 参数
    /// - `token_id`: 要交易的市场代币 ID
    /// - `max_spread_pct`: 最大可接受的价差百分比（例如 2.0 表示 2%）
    /// - `min_order_size`: 最小订单大小
    /// - `max_order_size`: 最大订单大小
    /// - `stale_threshold`: 数据过时阈值（秒）
    /// 
    /// # 返回
    /// 配置好的策略实例，包含订单簿管理器和订单执行引擎
    pub fn new(
        token_id: String,
        max_spread_pct: Decimal,
        min_order_size: Decimal,
        max_order_size: Decimal,
        stale_threshold: u64,
    ) -> Self {
        Self {
            token_id,
            max_spread_pct,
            min_order_size,
            max_order_size,
            stale_threshold,
            last_best_bid: None,
            last_best_ask: None,
            last_update: 0,
            // 创建订单簿管理器，保留前 100 个价格层级
            // 更多的层级可以捕获更多流动性，但占用更多内存
            book_manager: OrderBookManager::new(100),
            
            // 创建订单执行引擎
            // 参数：最小订单大小、最大滑点（2%）、费用率（5 bps = 0.05%）
            fill_engine: FillEngine::new(
                min_order_size,
                dec!(2.0), // 2% 最大滑点容忍度
                5,         // 5 bps（基点）费用率，即 0.05%
            ),
            stats: SnipeStats::default(),
        }
    }

    /// 处理市场数据更新
    /// 
    /// 这是策略的核心入口函数，处理来自 WebSocket 流的所有消息类型。
    /// 
    /// # 消息类型
    /// - `BookUpdate`: 订单簿更新（新增/修改/删除订单）
    /// - `Trade`: 成交记录（有人成交了订单）
    /// - `Heartbeat`: 心跳消息（用于检测连接是否正常和数据是否过时）
    /// 
    /// # 注意
    /// 在真实应用中，这些消息来自 WebSocket 连接，而不是模拟数据
    pub fn process_update(&mut self, message: StreamMessage) -> Result<()> {
        match message {
            // 处理订单簿更新：更新本地订单簿并检查交易机会
            StreamMessage::BookUpdate { data } => {
                if data.token_id == self.token_id {
                    self.process_book_update(data)?;
                }
            },

            // 处理成交记录：更新统计信息
            StreamMessage::Trade { data } => {
                if data.token_id == self.token_id {
                    self.process_trade(data)?;
                }
            },

            // 处理心跳：检查数据是否过时
            StreamMessage::Heartbeat { timestamp: _ } => {
                self.check_stale_quotes()?;
            },
            
            _ => {},
        }
        Ok(())
    }

    /// 处理订单簿更新
    /// 
    /// 当收到新的订单簿更新时：
    /// 1. 确保该代币的订单簿存在
    /// 2. 将更新应用到本地订单簿
    /// 3. 更新最佳买卖价格
    /// 4. 检查是否有交易机会
    /// 
    /// # 参数
    /// - `delta`: 订单簿增量更新（包含价格、数量、方向等信息）
    fn process_book_update(&mut self, delta: OrderDelta) -> Result<()> {
        // 确保订单簿存在（如果不存在则创建）
        self.book_manager.get_or_create_book(&self.token_id)?;

        // 将增量更新应用到本地订单簿
        // 这使用了 polyfill-rs 的高性能固定点算法，速度非常快
        self.book_manager.apply_delta(delta.clone())?;

        // 获取更新后的订单簿状态
        let book = self.book_manager.get_book(&self.token_id)?;

        // 更新最佳买价（订单簿买方的最高价格，即 bids 的第一项）
        if let Some(best_bid) = book.bids.first() {
            self.last_best_bid = Some(best_bid.price);
        }
        // 更新最佳卖价（订单簿卖方的最低价格，即 asks 的第一项）
        if let Some(best_ask) = book.asks.first() {
            self.last_best_ask = Some(best_ask.price);
        }

        // 更新最后更新时间戳
        self.last_update = time::now_secs();

        // 检查是否有交易机会（价差是否满足条件）
        self.check_opportunities()?;

        Ok(())
    }

    /// 处理成交记录更新
    /// 
    /// 当市场中有订单成交时，更新统计信息。
    /// 在真实应用中，你可以检查这是否是自己的订单，并计算盈亏。
    /// 
    /// # 参数
    /// - `fill`: 成交事件（包含价格、数量、方向等信息）
    fn process_trade(&mut self, fill: FillEvent) -> Result<()> {
        info!(
            "成交: {} {} @ {} (数量: {})",
            fill.side.as_str(),  // "BUY" 或 "SELL"
            fill.token_id,       // 代币 ID
            fill.price,          // 成交价格
            fill.size            // 成交数量
        );

        // 更新总成交量统计
        self.stats.total_volume += fill.size;

        // 如果这是我们的订单，计算盈亏
        // （在真实实现中，你需要跟踪自己的订单 ID）
        // 例如：
        // if self.is_my_order(&fill.order_id) {
        //     let pnl = self.calculate_pnl(&fill);
        //     self.stats.total_pnl += pnl;
        // }

        Ok(())
    }

    /// 检查交易机会
    /// 
    /// 核心逻辑：计算当前买卖价差，如果价差足够小（满足条件），
    /// 就执行交易来捕捉这个价差。
    /// 
    /// # 策略逻辑
    /// - 价差 = (最佳卖价 - 最佳买价) / 最佳买价 * 100%
    /// - 如果价差 <= max_spread_pct，说明市场流动性好，有机会快速成交
    /// - 此时可以买入然后立即卖出（或反之），赚取价差
    fn check_opportunities(&mut self) -> Result<()> {
        // 获取最佳买卖价格
        let (bid, ask) = match (self.last_best_bid, self.last_best_ask) {
            (Some(bid), Some(ask)) => (bid, ask),
            _ => return Ok(()), // 没有流动性，无法交易
        };

        // 计算价差百分比
        // 价差 = (卖价 - 买价) / 买价 * 100%
        // 例如：买价 0.50，卖价 0.51，价差 = (0.51-0.50)/0.50*100 = 2%
        let spread_pct = match (bid, ask) {
            (bid, ask) if bid > dec!(0) && ask > bid => (ask - bid) / bid * dec!(100),
            _ => return Ok(()), // 价格无效或市场交叉（买价 > 卖价，异常情况）
        };

        // 检查价差是否满足我们的交易条件
        // 价差越小，说明市场越紧密，我们越容易捕捉到机会
        if spread_pct <= self.max_spread_pct {
            // 发现交易机会！
            self.stats.opportunities_detected += 1;

            info!(
                "发现交易机会: 价差 {}% (目标: {}%)",
                spread_pct, self.max_spread_pct
            );

            // 执行狙击订单（快速买入或卖出）
            self.execute_snipe_order(bid, ask)?;
        }

        Ok(())
    }

    /// 执行狙击订单
    /// 
    /// 这是策略的执行部分，当发现交易机会时：
    /// 1. 确定订单大小（在最小和最大值之间）
    /// 2. 确定交易方向（买入或卖出）
    /// 3. 创建市价单请求
    /// 4. 使用 FillEngine 模拟执行订单
    /// 
    /// # 参数
    /// - `bid`: 当前最佳买价
    /// - `ask`: 当前最佳卖价
    /// 
    /// # 注意
    /// 这是一个示例实现，实际交易时你需要：
    /// - 通过 ClobClient 提交真实订单
    /// - 处理订单状态更新
    /// - 实现风险管理（仓位限制、止损等）
    fn execute_snipe_order(&mut self, bid: Decimal, ask: Decimal) -> Result<()> {
        // 计算订单大小（在最小和最大值之间随机选择）
        // 在真实应用中，你可能根据可用资金、风险参数等来决定大小
        let random_factor = Decimal::from(rand::random::<u64>() % 100) / Decimal::from(100);
        let size =
            self.min_order_size + (self.max_order_size - self.min_order_size) * random_factor;

        // 根据市场条件决定交易方向
        let side = if bid > ask {
            // 市场交叉（买价 > 卖价，异常情况），选择卖出
            Side::SELL
        } else {
            // 正常市场，选择买入（假设我们会立即卖出套利）
            Side::BUY
        };

        // 创建市价单请求
        // 市价单会以当前最优价格立即执行，速度快但可能有滑点
        let request = MarketOrderRequest {
            token_id: self.token_id.clone(),
            side,                            // 买入或卖出
            amount: size,                    // 订单金额
            slippage_tolerance: Some(dec!(1.0)), // 1% 滑点容忍度（允许实际成交价偏离预期 1%）
            client_id: Some(format!("snipe_{}", time::now_millis())), // 客户端订单 ID（用于追踪）
        };

        // 获取当前订单簿状态用于执行模拟
        // 注意：这里是为了演示，在真实应用中，订单执行由交易所完成
        let book = self.book_manager.get_book(&self.token_id)?;
        let mut book_impl = polyfill_rs::book::OrderBook::new(self.token_id.clone(), 100);

        // 将订单簿数据转换为内部格式（供 FillEngine 使用）
        // 复制所有买单层级
        for level in &book.bids {
            book_impl.apply_delta(OrderDelta {
                token_id: self.token_id.clone(),
                timestamp: chrono::Utc::now(),
                side: Side::BUY,
                price: level.price,
                size: level.size,
                sequence: 1,
            })?;
        }

        // 复制所有卖单层级
        for level in &book.asks {
            book_impl.apply_delta(OrderDelta {
                token_id: self.token_id.clone(),
                timestamp: chrono::Utc::now(),
                side: Side::SELL,
                price: level.price,
                size: level.size,
                sequence: 2,
            })?;
        }

        // 执行订单（模拟）
        // FillEngine 会根据订单簿计算：
        // - 实际成交价格（可能在多个价格层级上成交）
        // - 总成交数量
        // - 滑点（实际价格 vs 预期价格）
        // - 手续费
        let start_time = std::time::Instant::now();
        let result = self
            .fill_engine
            .execute_market_order(&request, &book_impl)?;
        let fill_time = start_time.elapsed().as_millis() as f64; // 记录执行耗时

        // 更新统计信息
        self.stats.orders_placed += 1; // 订单已提交
        if result.status == FillStatus::Filled {
            self.stats.orders_filled += 1; // 订单已完全成交
        }

        // 更新平均成交时间（使用移动平均）
        // 公式：新平均值 = (旧平均值 * (n-1) + 新值) / n
        let total_time =
            self.stats.avg_fill_time_ms * (self.stats.orders_filled - 1) as f64 + fill_time;
        self.stats.avg_fill_time_ms = total_time / self.stats.orders_filled as f64;

        info!(
            "狙击订单已执行: {} {} @ {} (成交时间: {}ms)",
            result.total_size,      // 成交总数量
            side.as_str(),          // 交易方向
            result.average_price,   // 平均成交价格
            fill_time               // 执行耗时（毫秒）
        );

        Ok(())
    }

    /// 检查过时的报价数据
    /// 
    /// 如果订单簿数据太久没有更新，可能意味着：
    /// - WebSocket 连接断开
    /// - 市场流动性很差
    /// - 数据源有问题
    /// 
    /// 在这种情况下继续交易是危险的，因为订单簿可能已经过时。
    /// 
    /// # 处理策略
    /// 当检测到数据过时时，应该：
    /// - 取消所有待处理订单（避免基于错误数据交易）
    /// - 切换到备用数据源（如果有）
    /// - 减少仓位大小或暂停交易
    /// - 尝试重新连接数据流
    fn check_stale_quotes(&mut self) -> Result<()> {
        let now = time::now_secs();
        let age = now.saturating_sub(self.last_update); // 数据年龄（秒）

        // 如果数据超过阈值未更新，认为数据已过时
        if age > self.stale_threshold {
            warn!(
                "检测到过时报价: 数据已 {} 秒未更新 (阈值: {} 秒)",
                age, self.stale_threshold
            );

            // 在真实实现中，你可能会：
            // - 取消所有待处理订单：self.cancel_all_pending_orders()?;
            // - 切换到备用数据源：self.switch_to_backup_data_source()?;
            // - 减少仓位大小：self.reduce_position_sizes()?;
            // - 临时停止交易：self.pause_trading()?;
            // - 尝试重新连接：self.reconnect_websocket().await?;
        }

        Ok(())
    }

    /// 获取当前统计数据
    /// 
    /// 返回策略运行过程中的所有统计信息，用于监控和评估策略表现。
    pub fn get_stats(&self) -> &SnipeStats {
        &self.stats
    }
}

/// 模拟市场数据生成器（用于测试和演示）
/// 
/// 这个结构体生成模拟的市场数据，用于在没有真实 WebSocket 连接的情况下测试策略。
/// 
/// # 注意
/// 在真实应用中，你应该使用 WebSocketStream 连接真实的市场数据源：
/// ```rust
/// let mut stream = WebSocketStream::new("wss://clob.polymarket.com/ws");
/// stream.subscribe_market_channel(vec!["token_id".to_string()]).await?;
/// while let Some(message) = stream.next().await {
///     strategy.process_update(message?)?;
/// }
/// ```
struct MockMarketData {
    /// 代币 ID
    token_id: String,
    /// 基础价格（模拟价格围绕这个价格波动）
    base_price: Decimal,
    /// 波动率（价格变动的幅度，例如 0.01 表示 1%）
    volatility: Decimal,
    /// 序列号（用于追踪消息顺序）
    sequence: u64,
}

impl MockMarketData {
    fn new(token_id: String, base_price: Decimal) -> Self {
        Self {
            token_id,
            base_price,
            volatility: dec!(0.01), // 1% volatility
            sequence: 0,
        }
    }

    /// 生成一个模拟的订单簿更新消息
    /// 
    /// 模拟真实市场数据的特征：
    /// - 价格围绕基础价格随机波动
    /// - 随机选择买单或卖单
    /// - 随机数量
    /// 
    /// # 返回
    /// 一个 StreamMessage::BookUpdate 消息，格式与真实 WebSocket 消息相同
    fn generate_update(&mut self) -> StreamMessage {
        self.sequence += 1; // 递增序列号

        // 生成随机价格变动
        // random_factor 在 -0.5 到 0.5 之间
        let random_factor = Decimal::from(rand::random::<i64>() % 100 - 50) / Decimal::from(100);
        let price_change = random_factor * Decimal::from(2) * self.volatility;
        // 新价格 = 基础价格 * (1 + 价格变动)
        // 例如：base=0.50, volatility=0.01, random_factor=0.3
        // price_change = 0.3 * 2 * 0.01 = 0.006
        // new_price = 0.50 * (1 + 0.006) = 0.503
        let new_price = self.base_price * (Decimal::from(1) + price_change);

        // 随机生成订单簿更新（买单或卖单）
        let side = if rand::random::<bool>() {
            Side::BUY   // 随机选择买单
        } else {
            Side::SELL  // 或卖单
        };
        // 随机数量（100 到 1100 之间）
        let size = Decimal::from(rand::random::<u64>() % 1000 + 100);

        // 构造订单簿更新消息
        StreamMessage::BookUpdate {
            data: OrderDelta {
                token_id: self.token_id.clone(),
                timestamp: chrono::Utc::now(),
                side,
                price: new_price,
                size,
                sequence: self.sequence,
            },
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统（使用 tracing 库）
    tracing_subscriber::fmt::init();

    info!("启动狙击交易示例...");

    // 创建狙击策略实例
    // 参数说明：
    // - token_id: "12345" - 示例代币 ID（真实应用中应该是实际的代币 ID）
    // - max_spread_pct: 2.0 - 最大价差 2%（只交易价差 <= 2% 的机会）
    // - min_order_size: 10 - 最小订单大小 $10
    // - max_order_size: 100 - 最大订单大小 $100
    // - stale_threshold: 5 - 数据过时阈值 5 秒
    let mut strategy = SnipeStrategy::new(
        "12345".to_string(), // 示例代币 ID
        dec!(2.0),           // 2% 最大价差
        dec!(10),            // 最小订单大小
        dec!(100),           // 最大订单大小
        5,                   // 5 秒过时阈值
    );

    // 创建模拟市场数据生成器
    // 注意：这是用于演示的模拟数据
    // 真实应用中，你应该使用 WebSocketStream 连接真实市场数据：
    //
    // ```rust
    // use polyfill_rs::{WebSocketStream, StreamMessage};
    // 
    // // 1. 创建 WebSocket 连接
    // let mut stream = WebSocketStream::new("wss://clob.polymarket.com/ws");
    // 
    // // 2. 如果需要认证（对于私有频道）
    // let auth = WssAuth {
    //     address: "your_eth_address".to_string(),
    //     signature: "your_signature".to_string(),
    //     timestamp: chrono::Utc::now().timestamp() as u64,
    //     nonce: "random_nonce".to_string(),
    // };
    // stream = stream.with_auth(auth);
    // 
    // // 3. 订阅市场数据频道
    // stream.subscribe_market_channel(vec!["12345".to_string()]).await?;
    // 
    // // 4. 处理实时消息
    // while let Some(message) = stream.next().await {
    //     match message? {
    //         StreamMessage::MarketBookUpdate { data } => {
    //             // 转换为策略需要的格式
    //             strategy.process_update(StreamMessage::BookUpdate { data })?;
    //         },
    //         StreamMessage::MarketTrade { data } => {
    //             strategy.process_update(StreamMessage::Trade { data })?;
    //         },
    //         StreamMessage::Heartbeat { timestamp } => {
    //             strategy.process_update(StreamMessage::Heartbeat { timestamp })?;
    //         },
    //         _ => {}
    //     }
    // }
    // ```
    let mut market_data = MockMarketData::new(
        "12345".to_string(),
        dec!(0.5), // 基础价格 $0.50
    );

    // 模拟市场数据流（处理 100 条消息）
    let mut message_count = 0;
    let max_messages = 100;

    while message_count < max_messages {
        // 生成模拟的市场更新
        let update = market_data.generate_update();

        // 处理更新（策略会检查是否有交易机会并执行）
        if let Err(e) = strategy.process_update(update) {
            error!("处理更新时出错: {}", e);
        }

        // 每处理 10 条消息打印一次统计信息
        if message_count % 10 == 0 {
            let stats = strategy.get_stats();
            info!(
                "统计: {} 个机会, {} 个订单已提交, {} 个已成交, 平均成交时间: {:.2}ms",
                stats.opportunities_detected,  // 检测到的机会数
                stats.orders_placed,           // 已提交的订单数
                stats.orders_filled,           // 已成交的订单数
                stats.avg_fill_time_ms         // 平均成交时间（毫秒）
            );
        }

        message_count += 1;
        // 每 100 毫秒处理一条消息（模拟真实的 WebSocket 消息频率）
        sleep(Duration::from_millis(100)).await;
    }

    // 打印最终统计信息
    let final_stats = strategy.get_stats();
    info!("最终统计:");
    info!(
        "  检测到的机会: {}",
        final_stats.opportunities_detected
    );
    info!("  已提交订单: {}", final_stats.orders_placed);
    info!("  已成交订单: {}", final_stats.orders_filled);
    info!("  总交易量: {}", final_stats.total_volume);
    info!("  平均成交时间: {:.2}ms", final_stats.avg_fill_time_ms);

    info!("狙击交易示例完成！");
    Ok(())
}
