//! Integration test: encode a real Fusion order and write calldata to a fixture
//! for Foundry fork-test verification.
//!
//! Run:  `cargo test -p backrunner encode_order -- --nocapture`
//! Then: `forge test --match-test test_encodedFill -vvvv`  (in contracts/)

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, U256};
    use anyhow::{Context, Result};
    use crate::abi::{build_settle_calldata, RawOrderFields, SettleParams};
    use crate::client::{FusionExtOrder, IAmountGetter, ILopSimulate};

    fn h(s: &str) -> Result<Vec<u8>> {
        hex::decode(s.replace([' ', '\n'], "")).context("hex decode")
    }
    fn u256h(s: &str) -> Result<U256> {
        Ok(U256::from_be_slice(&h(s)?))
    }

    // ERC-1271 signature (256 bytes) from smoke test at block 25222660
    const SIGNATURE_HEX: &str = concat!(
        "ddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090",
        "000000000000000000000000d27cc478689bea4dafe2ab7e486944d775e539a3",
        "000000000000000000000000399740157391a9f1bf4e9921a8834f9bc8f2678e",
        "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
        "000000000000000000000000dac17f958d2ee523a2206206994597c13d831ec7",
        "0000000000000000000000000000000000000000000000000950efcb15b84000",
        "000000000000000000000000000000000000000000000000000000004f25fe07",
        "8a00000000000000000000001254000016006a1d816300000000000000000000",
    );

    // Extension (459 bytes) from the 1inch API
    const EXTENSION_HEX: &str = concat!(
        "000001ab000000e6000000e6000000e6000000e6000000730000000000000000",
        "399740157391a9f1bf4e9921a8834f9bc8f2678e000c08000001506a1d80a300",
        "00b400d0630200bd220090000c080024012c6400006406b09498030ae3416b66",
        "dc74db31d09524fa87b1f76ea9a11ae13b29f5c555d18bd45f0b94f54a968fc9",
        "0ed87a54c23dc480b395770895ad27ad6b0d95399740157391a9f1bf4e9921a8",
        "834f9bc8f2678e000c08000001506a1d80a30000b400d0630200bd220090000c",
        "080024012c6400006406b09498030ae3416b66dc74db31d09524fa87b1f76ea9",
        "a11ae13b29f5c555d18bd45f0b94f54a968fc90ed87a54c23dc480b395770895",
        "ad27ad6b0d95399740157391a9f1bf4e9921a8834f9bc8f2678e0190cbe4bdd5",
        "38d6e9b379bff5fe72c3d67a521de590cbe4bdd538d6e9b379bff5fe72c3d67a",
        "521de5d27cc478689bea4dafe2ab7e486944d775e539a3012c640000646a1d80",
        "5b06b09498030ae3416b66dc000074db31d09524fa87b1f700006ea9a11ae13b",
        "29f5c5550000d18bd45f0b94f54a968f0000c90ed87a54c23dc480b300009577",
        "0895ad27ad6b0d95000000000000000000000000000000000000000000000000",
        "0000000000004f92159300",
    );

    // Fynd sequentialSwap calldata: WETH→USDC→USDT (644 bytes, from `fynd_tx` at block 25222660)
    const SWAP_CALLDATA_HEX: &str = concat!(
        "6fc8683a0000000000000000000000000000000000000000000000000950efcb",
        "15b84000000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead908",
        "3c756cc2000000000000000000000000dac17f958d2ee523a2206206994597c1",
        "3d831ec700000000000000000000000000000000000000000000000000000000",
        "4f13475700000000000000000000000000000000000000000000b09498030ae3",
        "416b66dc00000000000000000000000000000000000000000000000000000000",
        "000000e000000000000000000000000000000000000000000000000000000000",
        "000001a000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "00000000ffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ffffffff00000000000000000000000000000000000000000000000000000000",
        "000000a000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "000000aa0054667cb014f2b3c470b53089d984c3cca840d23052c02aaa39b223",
        "fe8d0a0e5c4f27ead9083c756cc2a0b86991c6218b36c1d19d4a2e9eb0ce3606",
        "eb48000064e0554a476a092703abdb3ef35c80e0d76d32939f000052ab081cbb",
        "3c88219a030928ece277fead99cab742667701e51b4d1ca244f17c78f7ab8744",
        "b4c99f9b01a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48dac17f958d2ee5",
        "23a2206206994597c13d831ec700000000000000000000000000000000000000",
        "00000000",
    );

    /// Real ERC-1271 Fusion order captured from smoke test at block 25222660.
    /// Order: WETH → USDT, maker = `0xc7ae508ddc86d6acfeb80c3f0e972d1a22bacaad`
    /// Router comes from `fynd_tx.to()` = `0xdA892C989d07A18B5DD3F392d949f00dF15C5736`
    #[test]
    fn encode_order_and_write_fixture() -> Result<()> {
        let order = RawOrderFields {
            salt:          u256h("ddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090")?,
            maker:         u256h("000000000000000000000000c7ae508ddc86d6acfeb80c3f0e972d1a22bacaad")?,
            receiver:      u256h("000000000000000000000000399740157391a9f1bf4e9921a8834f9bc8f2678e")?,
            maker_asset:   u256h("000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")?,
            taker_asset:   u256h("000000000000000000000000dac17f958d2ee523a2206206994597c13d831ec7")?,
            making_amount: U256::from(671_300_000_000_000_000_u128), // 0.6713 WETH
            taking_amount: U256::from(1_327_889_927_u64),             // ~1327 USDT
            maker_traits:  u256h("8a00000000000000000000001254000016006a1d816300000000000000000000")?,
        };
        let signature = h(SIGNATURE_HEX)?;
        anyhow::ensure!(signature.len() == 256, "signature length: {}", signature.len());
        let extension = h(EXTENSION_HEX)?;
        anyhow::ensure!(extension.len() == 459, "extension must be 459 bytes");
        let primary_swap_calldata = h(SWAP_CALLDATA_HEX)?;
        anyhow::ensure!(primary_swap_calldata.len() == 644, "swap calldata must be 644 bytes");

        let resolver: Address = "0x00000000000000000000b09498030ae3416b66Dc".parse()?;
        let fynd_router: Address = "0xdA892C989d07A18B5DD3F392d949f00dF15C5736".parse()?;

        let params = SettleParams {
            order_fields: &order,
            signature: &signature,
            extension: &extension,
            taking_amount: U256::from(1_327_889_927_u64),
            fill_amount: U256::from(671_300_000_000_000_000_u128),
            router: fynd_router,
            primary_swap_calldata: &primary_swap_calldata,
            surplus_calldata: &[],
            resolver_address: resolver,
        };
        let calldata = build_settle_calldata(&params);

        let fixture_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/test/fixtures/encoded_fill.hex"
        );
        let parent = std::path::Path::new(fixture_path)
            .parent()
            .context("fixture path has no parent")?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(fixture_path, hex::encode(&calldata))?;

        anyhow::ensure!(calldata[..4] == [0x09u8, 0x65, 0xd0, 0x4b], "settleOrders selector");

        let inner_len = u32::from_be_bytes(calldata[64..68].try_into()?) as usize;
        let inner = &calldata[68..68 + inner_len];
        anyhow::ensure!(inner[..4] == [0x56u8, 0xa7, 0x58, 0x68], "fillContractOrderArgs selector");

        let tt = U256::from_be_slice(&inner[324..356]);
        let ext_len_enc = ((tt >> U256::from(224u64)) & U256::from(0x00ff_ffffu64)).to::<usize>();
        let int_len_enc = ((tt >> U256::from(200u64)) & U256::from(0x00ff_ffffu64)).to::<usize>();
        let has_target = ((tt >> U256::from(251u64)) & U256::from(1u64)).to::<u64>();

        anyhow::ensure!(ext_len_enc == 459, "extensionLength in takerTraits: {ext_len_enc}");
        anyhow::ensure!(int_len_enc > 0, "interactionLength must be > 0");
        anyhow::ensure!(has_target == 1, "ARGS_HAS_TARGET not set");

        let args_abs_off = U256::from_be_slice(&inner[356..388]).to::<usize>();
        let args_data_start = 4 + args_abs_off + 32;
        let args_resolver = &inner[args_data_start..args_data_start + 20];
        anyhow::ensure!(
            hex::encode(args_resolver) == "00000000000000000000b09498030ae3416b66dc",
            "args_resolver mismatch"
        );

        let interaction_start = args_data_start + 20 + ext_len_enc;
        let interaction_resolver = &inner[interaction_start..interaction_start + 20];
        anyhow::ensure!(
            hex::encode(interaction_resolver) == "00000000000000000000b09498030ae3416b66dc",
            "interaction must start with resolver"
        );

        let router_bytes = &inner[interaction_start + 20 + 12..interaction_start + 20 + 32];
        anyhow::ensure!(
            hex::encode(router_bytes) == "da892c989d07a18b5dd3f392d949f00df15c5736",
            "Fynd router correctly encoded"
        );

        Ok(())
    }

    /// Live RPC test: call `getTakingAmount` on the Fusion extension at block 25222660
    /// using the exact order that was active at that block.
    ///
    /// Requires:  `ETH_RPC_URL` environment variable pointing to an archive node.
    /// Run with:  `cargo test -p backrunner get_taking_amount_rpc -- --ignored --nocapture`
    ///
    /// Success means our ABI encoding is accepted by the extension contract.
    /// The returned amount should be ≥ the order floor (`1_327_889_927` USDT).
    #[test]
    #[ignore = "requires ETH_RPC_URL pointing to an archive node"]
    fn get_taking_amount_rpc() -> Result<()> {
        use alloy::sol_types::{SolCall, SolError as _};

        let rpc_url = std::env::var("ETH_RPC_URL").context("ETH_RPC_URL not set")?;

        // Extension address lives at ext_hex[64..104].
        let ext_hex = EXTENSION_HEX;
        let ext_addr_bytes = h(&ext_hex[64..104])?;
        let ext_addr: Address = Address::from_slice(&ext_addr_bytes);

        // Order hash from the 1inch API (the `order_id` field, without 0x).
        let order_hash_bytes: [u8; 32] =
            h("ddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090")?
                .try_into()
                .map_err(|_| anyhow::anyhow!("order hash not 32 bytes"))?;

        let making_amount: u128 = 671_300_000_000_000_000;
        let floor_taking_amount: u128 = 1_327_889_927;

        // Extract TakingAmountData section extraData from the extension.
        // Header bytes [16:20] = TakingAmountData end offset (bits [127:96]).
        // Header bytes [20:24] = MakingAmountData end offset (bits [95:64]).
        // Section content starts at 32 + making_end; first 20 bytes = getter address, rest = extraData.
        let ext_bytes_clone = h(ext_hex)?;
        let making_end = u32::from_be_bytes(ext_bytes_clone[20..24].try_into()?) as usize;
        let taking_end = u32::from_be_bytes(ext_bytes_clone[16..20].try_into()?) as usize;
        let taking_extra_data = alloy::primitives::Bytes::copy_from_slice(
            &ext_bytes_clone[32 + making_end + 20 .. 32 + taking_end]
        );

        let call = IAmountGetter::getTakingAmountCall {
            order: FusionExtOrder {
                salt:          u256h("ddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090")?,
                maker:         u256h("000000000000000000000000c7ae508ddc86d6acfeb80c3f0e972d1a22bacaad")?,
                receiver:      u256h("000000000000000000000000399740157391a9f1bf4e9921a8834f9bc8f2678e")?,
                makerAsset:    u256h("000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")?,
                takerAsset:    u256h("000000000000000000000000dac17f958d2ee523a2206206994597c13d831ec7")?,
                makingAmount:  U256::from(making_amount),
                takingAmount:  U256::from(floor_taking_amount),
                makerTraits:   u256h("8a00000000000000000000001254000016006a1d816300000000000000000000")?,
            },
            extension:              alloy::primitives::Bytes::from(h(ext_hex)?),
            orderHash:              alloy::primitives::FixedBytes::from(order_hash_bytes),
            taker:                  "0x00000000000000000000b09498030ae3416b66Dc".parse()?,
            makingAmount:           U256::from(making_amount),
            remainingMakingAmount:  U256::from(making_amount),
            extraData:              taking_extra_data,
        };

        // Wrap in LOP.simulate(extension_addr, inner_calldata) — the LOP calls the
        // extension with msg.sender == LOP, which satisfies any caller check in the extension.
        // simulate() always reverts with SimulationResults(success, result).
        let lop_addr: alloy::primitives::Address =
            "0x111111125421cA6dc452d289314280a0f8842A65".parse()?;
        let simulate_call = ILopSimulate::simulateCall {
            target: ext_addr,
            data: alloy::primitives::Bytes::from(call.abi_encode()),
        };
        let lop_hex  = format!("0x{}", hex::encode(lop_addr.as_slice()));
        let encoded = simulate_call.abi_encode();
        let data_hex = format!("0x{}", hex::encode(&encoded));

        let block_tag = "0x180de04"; // 25222660 in hex

        // Print for manual curl debugging.
        // Write calldata to /tmp for manual curl debugging.
        let _ = std::fs::write("/tmp/simulate_calldata.hex", &data_hex);
        tracing::info!(
            target = %format!("0x{}", hex::encode(ext_addr.as_slice())),
            lop = %lop_hex,
            block = block_tag,
            inner_len = encoded.len(),
            calldata_written_to = "/tmp/simulate_calldata.hex",
            "simulate() calldata",
        );

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": lop_hex, "data": data_hex}, block_tag],
            "id": 1
        });

        let rt = tokio::runtime::Runtime::new()?;
        let json: serde_json::Value = rt.block_on(async {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .context("build client")?
                .post(&rpc_url)
                .json(&body)
                .send()
                .await
                .context("HTTP send")?
                .json()
                .await
                .context("JSON parse")
        })?;

        // simulate() always reverts — parse SimulationResults from the error data.
        let err = json.get("error").context("simulate() did not revert")?;
        let revert_str = err.get("data").and_then(|d| d.as_str())
            .context("no revert data in SimulationResults")?;
        let revert_bytes = h(revert_str.strip_prefix("0x").unwrap_or(revert_str))?;
        let sim = ILopSimulate::SimulationResults::abi_decode(&revert_bytes)
            .context("failed to decode SimulationResults")?;
        anyhow::ensure!(sim.success, "getTakingAmount inner call reverted: {:?}", sim.result);

        anyhow::ensure!(sim.result.len() >= 32, "result too short: {} bytes", sim.result.len());
        let taking_amount = U256::from_be_slice(&sim.result[..32]);
        tracing::info!(taking_amount = %taking_amount, floor = floor_taking_amount, "getTakingAmount result");
        anyhow::ensure!(
            taking_amount >= U256::from(floor_taking_amount),
            "taking amount {taking_amount} below floor {floor_taking_amount}",
        );

        Ok(())
    }
}
