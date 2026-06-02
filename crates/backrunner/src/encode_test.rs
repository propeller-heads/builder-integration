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
            taking_amount: 1_327_889_927_u128,
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
}
