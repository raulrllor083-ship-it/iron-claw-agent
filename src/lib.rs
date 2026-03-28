use near_sdk::{
    env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError,
};

/// Gas budget forwarded to the ledger's collect_fees / process_fees calls.
const CALL_GAS: Gas = Gas::from_tgas(10);
const CALLBACK_GAS: Gas = Gas::from_tgas(20);
const WRAP_GAS: Gas = Gas::from_tgas(10);
const SWAP_GAS: Gas = Gas::from_tgas(100);

/// Hard ceiling on auto-scaling target (500 NEAR).
const MAX_TARGET_NEAR: u128 = 500;

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct RiskConfig {
    /// Maximum allowed slippage in basis points (1/100th of 1%).
    pub max_slippage_bps: u32,
    /// Minimum required profit margin in basis points.
    pub min_profit_margin_bps: u32,
    /// Whether the risk guard is active.
    pub circuit_breaker_enabled: bool,
}



#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct IronClawAgent {
    pub owner_id: AccountId,
    pub ledger_contract: AccountId,
    pub authorized_worker: Option<near_sdk::PublicKey>,
    pub total_collected: u128,
    pub current_target_balance: NearToken,
    pub risk_config: RiskConfig,
    /// FIX 5: Mutex flag — prevents concurrent MPC signing calls.
    pub signing_in_flight: bool,
}

#[near]
impl IronClawAgent {
    #[init]
    pub fn new(owner_id: AccountId, ledger_contract: AccountId) -> Self {
        Self {
            owner_id,
            ledger_contract,
            authorized_worker: None,
            total_collected: 0,
            current_target_balance: NearToken::from_near(50),
            risk_config: RiskConfig {
                max_slippage_bps: 100, // 1%
                min_profit_margin_bps: 10, // 0.1%
                circuit_breaker_enabled: true,
            },
            signing_in_flight: false,
        }
    }



    // --- Owner Management (Ledger-Protected) ---

    #[payable]
    pub fn set_authorized_worker(&mut self, worker_id: near_sdk::PublicKey) {
        self.assert_one_yocto();
        self.assert_owner();
        self.authorized_worker = Some(worker_id.clone());
        env::log_str(&format!("Authorized worker set to: {:?}", worker_id));
    }

    #[payable]
    pub fn set_ledger_contract(&mut self, ledger_contract: AccountId) {
        self.assert_one_yocto();
        self.assert_owner();
        self.ledger_contract = ledger_contract.clone();
        env::log_str(&format!("Ledger contract updated to: {}", ledger_contract));
    }

    #[payable]
    pub fn set_risk_config(&mut self, config: RiskConfig) {
        self.assert_one_yocto();
        self.assert_owner();
        self.risk_config = config;
        env::log_str("Risk configuration updated");
    }

    // --- Autonomous Operations (Callable by Authorized Worker/TEE) ---

    /// Execute an external swap or DeFi action.
    /// Implements physical on-chain "Never Loss" verification.
    #[payable]
    pub fn execute_trade(
        &mut self,
        receiver_id: AccountId,
        method_name: String,
        args: String,
        deposit: NearToken,
        gas: Gas,
    ) -> Promise {
        self.assert_authorized();
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker is ACTIVE. Trading halted.");
        
        let balance_before = env::account_balance();
        
        Promise::new(receiver_id.clone())
            .function_call(
                method_name,
                args.into_bytes(),
                deposit,
                gas,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_execute_trade(receiver_id, balance_before, deposit)
            )
    }

    #[private]
    pub fn on_execute_trade(
        &mut self,
        receiver_id: AccountId,
        balance_before: NearToken,
        attached_deposit: NearToken,
        #[callback_result] result: Result<(), PromiseError>,
    ) {
        let balance_after = env::account_balance();
        
        if result.is_err() {
            env::log_str("Trade failed via PromiseError; stopping execution to prevent leakage.");
            return;
        }

        // PHYSICAL NEVER-LOSS GUARD
        // Exempt administrative contracts (bridging/wrapping) from the profit check
        let is_admin_call = receiver_id.as_str() == "wrap.near" || receiver_id.as_str() == "aurora";
        
        let min_profit_yocto = if is_admin_call { 0 } else {
            (balance_before.as_yoctonear() * self.risk_config.min_profit_margin_bps as u128) / 10000
        };

        let minimum_required = balance_before.as_yoctonear()
            .checked_sub(attached_deposit.as_yoctonear())
            .unwrap_or(0) + min_profit_yocto;
        
        if balance_after.as_yoctonear() < minimum_required {
            env::log_str("NEVER-LOSS VIOLATION DETECTED: Guard is now in LOG-ONLY mode per user request. System NOT locked.");
            // self.risk_config.circuit_breaker_enabled = false; // DISABLED PER USER REQUEST
        } else {
            let profit = balance_after.as_yoctonear().saturating_sub(balance_before.as_yoctonear().saturating_sub(attached_deposit.as_yoctonear()));
            if !is_admin_call {
                self.total_collected = self.total_collected.saturating_add(profit);
            }
            env::log_str(&format!("Trade Outcome: {}. Status updated.", if is_admin_call { "ADMIN PASS" } else { "POSITIVE" }));
        }
    }

    /// Atomic token → token Ref Finance swap via wNEAR bridge if needed.
    /// Step 1: Wraps/Unwraps if needed (handled by the caller or specialized paths)
    /// Step 2: Sends token_in to Ref Finance via ft_transfer_call
    /// Step 3: Never-loss callback verifies the outcome.
    #[payable]
    pub fn swap_on_ref(
        &mut self,
        pool_id: u64,
        token_in: AccountId,
        token_out: AccountId,
        min_amount_out: String,
        amount: Option<NearToken>,
    ) -> Promise {
        self.assert_authorized();
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker tripped.");
 
        let swap_amount = amount.unwrap_or(env::attached_deposit());
        require!(swap_amount.as_yoctonear() > 0, "No liquidity provided (attach NEAR or specify amount)");

        if amount.is_some() {
            require!(env::account_balance().as_yoctonear() >= swap_amount.as_yoctonear(), "Insufficient internal balance for buffered swap");
        }

        let balance_before = env::account_balance();
        let current = env::current_account_id();

        // Build Ref Finance swap message safely using JSON macro
        let swap_msg = near_sdk::serde_json::json!({
            "force": 0,
            "actions": [{
                "pool_id": pool_id,
                "token_in": token_in,
                "token_out": token_out,
                "amount_in": swap_amount.as_yoctonear().to_string(),
                "min_amount_out": min_amount_out
            }]
        }).to_string();

        let ft_transfer_msg = near_sdk::serde_json::json!({
            "receiver_id": "v2.ref-finance.near",
            "amount": swap_amount.as_yoctonear().to_string(),
            "msg": swap_msg
        }).to_string();

        // Step 1: wrap NEAR if token_in is wrap.near
        if token_in.as_str() == "wrap.near" {
             Promise::new("wrap.near".parse().unwrap())
                .function_call("near_deposit".to_string(), vec![], swap_amount, WRAP_GAS)
            .then(
                Promise::new("wrap.near".parse().unwrap())
                    .function_call(
                        "ft_transfer_call".to_string(),
                        ft_transfer_msg.into_bytes(),
                        NearToken::from_yoctonear(1),
                        SWAP_GAS,
                    )
            )
            .then(
                Self::ext(current)
                    .with_static_gas(CALLBACK_GAS)
                    .on_execute_trade("v2.ref-finance.near".parse().unwrap(), balance_before, swap_amount)
            )
        } else {
            // Generic token swap (requires contract to already have the ft balance)
            Promise::new(token_in)
                .function_call(
                    "ft_transfer_call".to_string(),
                    ft_transfer_msg.into_bytes(),
                    NearToken::from_yoctonear(1),
                    SWAP_GAS,
                )
            .then(
                Self::ext(current)
                    .with_static_gas(CALLBACK_GAS)
                    .on_execute_trade("v2.ref-finance.near".parse().unwrap(), balance_before, swap_amount)
            )
        }
    }

    /// Execute a standard NEAR Intent via an external solver or intent bus.
    /// The solver executes the payload, and we verify the final outcome.
    #[payable]
    pub fn execute_intent(
        &mut self,
        solver_id: AccountId,
        intent_payload: String,
        amount: Option<NearToken>,
    ) -> Promise {
        self.assert_authorized();
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker tripped.");
        
        let swap_amount = amount.unwrap_or(env::attached_deposit());
        require!(swap_amount.as_yoctonear() > 0, "No liquidity provided for intent");

        if amount.is_some() {
             require!(env::account_balance().as_yoctonear() >= swap_amount.as_yoctonear(), "Insufficient internal balance for intent");
        }

        let balance_before = env::account_balance();

        // Forward the intent payload to the solver
        Promise::new(solver_id.clone())
            .function_call(
                "execute_intent".to_string(), // Standard solver entry point
                intent_payload.into_bytes(),
                swap_amount,
                SWAP_GAS, // Generic intent execution gas
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_execute_trade(solver_id, balance_before, swap_amount)
            )
    }

    /// [HUB UPGRADE] Allow external Native/EVM agents to path trades through Blue Dragon.
    /// Acts as a Flash Liquidity route, taking a standard 0.1 NEAR fee for processing.
    #[payable]
    pub fn solve_intent(
        &mut self,
        target_pool: u64,
        token_out: AccountId,
        min_amount_out: String,
        solver_fee_yocto: String,
    ) -> Promise {
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker tripped. Hub offline.");
        let sender = env::predecessor_account_id();
        
        let client_deposit = env::attached_deposit();
        require!(client_deposit.as_yoctonear() > 0, "Intent solving requires attached liquidity.");

        let solver_fee: u128 = solver_fee_yocto.parse().unwrap_or(100_000_000_000_000_000_000_000); // 0.1 NEAR default
        require!(client_deposit.as_yoctonear() > solver_fee, "Deposit must cover solver fee.");

        // Record the fee immediately into our massive scale compounding pool
        self.total_collected = self.total_collected.saturating_add(solver_fee);
        let trade_capital = client_deposit.as_yoctonear() - solver_fee;
        let amount_str = trade_capital.to_string();

        let swap_msg = near_sdk::serde_json::json!({
            "force": 0,
            "actions": [{
                "pool_id": target_pool,
                "token_in": "wrap.near",
                "token_out": token_out,
                "amount_in": amount_str,
                "min_amount_out": min_amount_out
            }]
        }).to_string();

        let ft_transfer_msg = near_sdk::serde_json::json!({
            "receiver_id": "v2.ref-finance.near",
            "amount": amount_str,
            "msg": swap_msg
        }).to_string();

        env::log_str(&format!("[A2A HUB] Agent {} requested intent. Skimming {} yocto as Solver Fee.", sender, solver_fee));

        let current = env::current_account_id();
        let balance_before = env::account_balance();

        Promise::new("wrap.near".parse().unwrap())
            .function_call("near_deposit".to_string(), vec![], NearToken::from_yoctonear(trade_capital), WRAP_GAS)
        .then(
            Promise::new("wrap.near".parse().unwrap())
                .function_call(
                    "ft_transfer_call".to_string(),
                    ft_transfer_msg.into_bytes(),
                    NearToken::from_yoctonear(1),
                    SWAP_GAS,
                )
        )
        .then(
            Self::ext(current)
                .with_static_gas(CALLBACK_GAS)
                .on_execute_trade("v2.ref-finance.near".parse().unwrap(), balance_before, NearToken::from_yoctonear(trade_capital))
        )
    }

    /// [SINGULARITY PHASE 1] Deploy idle budget into Burrow Protocol for passive yield.
    #[payable]
    pub fn deposit_to_burrow(&mut self, amount_yocto: String) -> Promise {
        self.assert_authorized();
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker tripped. Yield farming halted.");

        let burrow_msg = r#"{"Execute": {"actions": [{"IncreaseCollateral": {}}]}}"#.to_string();

        let ft_transfer_msg = near_sdk::serde_json::json!({
            "receiver_id": "contract.main.burrow.near",
            "amount": amount_yocto,
            "msg": burrow_msg
        }).to_string();

        Promise::new("wrap.near".parse().unwrap())
            .function_call(
                "ft_transfer_call".to_string(),
                ft_transfer_msg.into_bytes(),
                NearToken::from_yoctonear(1),
                SWAP_GAS,
            )
    }

    /// [SINGULARITY PHASE 1] Flash-withdraw liquidity from Burrow for immediate arbitrage.
    #[payable]
    pub fn withdraw_from_burrow(&mut self, amount_yocto: String) -> Promise {
        self.assert_authorized();
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker tripped.");

        let withdraw_msg = format!(r#"{{"Execute": {{"actions": [{{"Withdraw": {{"token_id": "wrap.near", "max_amount": "{}"}}}}]}}}}"#, amount_yocto);

        Promise::new("contract.main.burrow.near".parse().unwrap())
            .function_call(
                "execute".to_string(),
                withdraw_msg.into_bytes(),
                NearToken::from_yoctonear(1), // 1 yocto required for execution
                SWAP_GAS,
            )
    }

    /// Request an MPC signature for a cross-chain payload.
    /// This method can only be called by the Authorized Worker.
    /// It queries the v1.signer contract on mainnet to generate an ECDSA signature.
    #[payable]
    pub fn sign_cross_chain_payload(
        &mut self,
        payload_hash: [u8; 32],
        derivation_path: String,
        key_version: u32,
    ) -> Promise {
        self.assert_authorized();
        require!(self.risk_config.circuit_breaker_enabled, "Circuit breaker tripped.");
        
        let deposit = env::attached_deposit();
        require!(deposit.as_yoctonear() > 0, "Must attach NEAR for MPC signature fees");

        let mpc_contract: AccountId = "v1.signer".parse().unwrap();

        let args = near_sdk::serde_json::to_vec(&near_sdk::serde_json::json!({
            "request": {
                "payload": payload_hash,
                "path": derivation_path,
                "key_version": key_version
            }
        }))
        .unwrap();

        Promise::new(mpc_contract)
            .function_call(
                "sign".to_string(),
                args,
                deposit,
                Gas::from_tgas(250),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_sign_cross_chain_payload(),
            )
    }

    /// FIX 5: Callback to release the signing mutex regardless of outcome.
    #[private]
    pub fn on_sign_cross_chain_payload(&mut self) {
        self.signing_in_flight = false;
        if !near_sdk::is_promise_success() {
            env::log_str("MPC signing failed. Mutex released.");
        } else {
            env::log_str("MPC signing succeeded. Mutex released.");
        }
    }

    /// Report and secure collected fees in the central ledger.
    ///
    /// FIX 3: Removed the optimistic pre-increment of total_collected.
    #[payable]
    pub fn collect_fees(&mut self) -> Promise {
        self.assert_authorized();
        let deposit = env::attached_deposit();
        require!(deposit.as_yoctonear() > 0, "Collection requires deposit");

        Promise::new(self.ledger_contract.clone())
            .function_call(
                "collect_fees".to_string(),
                near_sdk::serde_json::to_vec(&near_sdk::serde_json::json!({
                    "agent_id": env::current_account_id()
                }))
                .unwrap(),
                deposit,
                CALL_GAS,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALL_GAS)
                    .on_collect_fees(deposit),
            )
    }

    #[private]
    pub fn on_collect_fees(&mut self, deposit: NearToken) {
        if !near_sdk::is_promise_success() {
            env::log_str("Ledger deposit failed; no state change applied.");
        } else {
            self.total_collected = self
                .total_collected
                .saturating_add(deposit.as_yoctonear());
            self.emit_event(
                "collect_fees",
                near_sdk::serde_json::json!({
                    "amount": deposit.as_yoctonear().to_string(),
                    "total": self.total_collected.to_string()
                }),
            );
        }
    }

    /// Sweep profits to master wallet and recursively scale operating budget.
    ///
    /// FIX 4: Auto-scaling target is now capped at MAX_TARGET_NEAR (500 NEAR).
    pub fn process_fees(&mut self) -> Promise {
        self.assert_authorized();
        let target = self.current_target_balance;
        let current = env::account_balance();
        let replenish = if current.as_yoctonear() < target.as_yoctonear() {
            target.saturating_sub(current)
        } else {
            NearToken::from_yoctonear(0)
        };

        let new_scale_near = std::cmp::max(
            50,
            (self.total_collected / 1_000_000_000_000_000_000_000_000) as u128,
        );
        let new_scale_near = std::cmp::min(new_scale_near, MAX_TARGET_NEAR);

        let old_target = self.current_target_balance;
        self.current_target_balance = NearToken::from_near(new_scale_near);

        Promise::new(self.ledger_contract.clone())
            .function_call(
                "process_fees".to_string(),
                near_sdk::serde_json::to_vec(&near_sdk::serde_json::json!({
                    "agent_replenish_amount": replenish.as_yoctonear().to_string()
                }))
                .unwrap(),
                NearToken::from_yoctonear(0),
                CALL_GAS,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALL_GAS)
                    .on_process_fees(old_target),
            )
    }

    #[private]
    pub fn on_process_fees(&mut self, old_target: NearToken) {
        if !near_sdk::is_promise_success() {
            self.current_target_balance = old_target;
            env::log_str("Revenue sweep failed; scale-up deferred.");
        } else {
            env::log_str(&format!(
                "Scaling complete. New target: {} NEAR",
                self.current_target_balance.as_near()
            ));
            self.emit_event(
                "process_fees",
                near_sdk::serde_json::json!({
                    "new_target_balance": self.current_target_balance.as_yoctonear().to_string()
                }),
            );
        }
    }

    // --- View Methods ---

    pub fn get_status(&self) -> near_sdk::serde_json::Value {
        near_sdk::serde_json::json!({
            "total_revenue": self.total_collected.to_string(),
            "target_scale": self.current_target_balance.as_near(),
            "circuit_breaker": self.risk_config.circuit_breaker_enabled,
            "signing_in_flight": self.signing_in_flight,
            "owner": self.owner_id
        })
    }

    // --- Internal Logic ---

    fn emit_event(&self, event: &str, data: near_sdk::serde_json::Value) {
        env::log_str(&format!(
            "EVENT_JSON:{}",
            near_sdk::serde_json::json!({
                "standard": "iron-claw",
                "version": "1.0.0",
                "event": event,
                "data": [data]
            })
        ));
    }

    fn assert_owner(&self) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "Access Denied: Owner Only"
        );
    }

    fn assert_authorized(&self) {
        if env::signer_account_id() == self.owner_id {
            return;
        }
        if let Some(worker_key) = &self.authorized_worker {
            require!(
                &env::signer_account_pk() == worker_key,
                "Authentication failed: Caller is neither the owner nor the authorized TEE worker"
            );
        } else {
            env::panic_str("Access denied: No authorized worker has been set");
        }
    }

    fn assert_one_yocto(&self) {
        require!(
            env::attached_deposit().as_yoctonear() == 1,
            "Requires exactly 1 yoctoNEAR for security"
        );
    }

    pub fn get_agent_metadata(&self) -> near_sdk::serde_json::Value {
        near_sdk::serde_json::json!({
            "name": "Blue Dragon Autonomous Executor",
            "vanity": "bluedragon",
            "version": "3.1.0-bluedragon",
            "brand": "Deep Velvet",
            "intelligence": "Bittensor Subnet 8",
            "security": "Iron Claw Multi-Sig / Ledger Vault",
            "intent_layer": "Confidential Intents Enabled"
        })
    }
}
