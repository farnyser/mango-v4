use crate::accounts_zerocopy::*;
use crate::error::MangoError;
use crate::group_seeds;
use crate::state::{compute_health_from_fixed_accounts, Bank, Group, HealthType, MangoAccount};
use crate::util::checked_math as cm;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions as tx_instructions;
use anchor_spl::token::{self, Token, TokenAccount};
use fixed::types::I80F48;

/// Sets up mango vaults for flash loan
///
/// In addition to these accounts, there must be a sequence of remaining_accounts:
/// 1. N banks
/// 2. N vaults, matching the banks
/// 3. N token accounts, where loaned funds are transfered
#[derive(Accounts)]
pub struct FlashLoan3Begin<'info> {
    pub group: AccountLoader<'info, Group>,
    pub token_program: Program<'info, Token>,

    #[account(address = tx_instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
}

/// Finalizes a flash loan
///
/// In addition to these accounts, there must be a sequence of remaining_accounts:
/// 1. health accounts
/// 2. N vaults, matching what was in FlashLoan3Begin
/// 3. N token accounts, matching what was in FlashLoan3Begin
#[derive(Accounts)]
pub struct FlashLoan3End<'info> {
    #[account(
        mut,
        has_one = owner,
    )]
    pub account: AccountLoader<'info, MangoAccount>,
    pub owner: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

pub fn flash_loan3_begin<'key, 'accounts, 'remaining, 'info>(
    ctx: Context<'key, 'accounts, 'remaining, 'info, FlashLoan3Begin<'info>>,
    loan_amounts: Vec<u64>,
) -> Result<()> {
    let num_loans = loan_amounts.len();
    require_eq!(
        ctx.remaining_accounts.len(),
        3 * num_loans,
        MangoError::SomeError
    );
    let banks = &ctx.remaining_accounts[..num_loans];
    let vaults = &ctx.remaining_accounts[num_loans..2 * num_loans];
    let token_accounts = &ctx.remaining_accounts[2 * num_loans..];

    let group = ctx.accounts.group.load()?;
    let group_seeds = group_seeds!(group);
    let seeds = [&group_seeds[..]];

    // Check that the banks and vaults correspond
    for (((bank_ai, vault_ai), token_account_ai), amount) in banks
        .iter()
        .zip(vaults.iter())
        .zip(token_accounts.iter())
        .zip(loan_amounts.iter())
    {
        let mut bank = bank_ai.load_mut::<Bank>()?;
        require_keys_eq!(bank.group, ctx.accounts.group.key());
        require_keys_eq!(bank.vault, *vault_ai.key);

        let token_account = Account::<TokenAccount>::try_from(token_account_ai)?;

        bank.flash_loan_approved_amount = *amount;
        bank.flash_loan_vault_initial = token_account.amount;

        // Transfer the loaned funds
        if *amount > 0 {
            let transfer_ctx = CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token::Transfer {
                    from: vault_ai.clone(),
                    to: token_account_ai.clone(),
                    authority: ctx.accounts.group.to_account_info(),
                },
            )
            .with_signer(&seeds);
            token::transfer(transfer_ctx, *amount)?;
        }
    }

    // Check if the other instructions in the transactions are compatible
    {
        let ixs = ctx.accounts.instructions.as_ref();
        let current_index = tx_instructions::load_current_index_checked(ixs)? as usize;

        // Forbid FlashLoan3Begin to be called from CPI (it does not have to be the first instruction)
        let current_ix = tx_instructions::load_instruction_at_checked(current_index, ixs)?;
        require_keys_eq!(
            current_ix.program_id,
            *ctx.program_id,
            MangoError::SomeError
        );

        // The only other mango instruction that must appear before the end of the tx is
        // the FlashLoan3End instruction. No other mango instructions are allowed.
        let mut index = current_index + 1;
        let mut found_end = false;
        loop {
            let ix = match tx_instructions::load_instruction_at_checked(index, ixs) {
                Ok(ix) => ix,
                Err(ProgramError::InvalidArgument) => break, // past the last instruction
                Err(e) => Err(e)?,
            };

            // Check that the mango program key is not used
            if ix.program_id == crate::id() {
                // must be the last mango ix -- this could possibly be relaxed, but right now
                // we need to guard against multiple FlashLoanEnds
                require!(!found_end, MangoError::SomeError);
                found_end = true;

                // must be the FlashLoan3End instruction
                require!(
                    &ix.data[0..8] == &[163, 231, 155, 56, 201, 68, 84, 148],
                    MangoError::SomeError
                );

                // check that the same vaults are passed
                let begin_accounts = &ctx.remaining_accounts[num_loans..];
                let end_accounts = &ix.accounts[ix.accounts.len() - 2 * num_loans..];
                for (begin_account, end_account) in begin_accounts.iter().zip(end_accounts.iter()) {
                    require_keys_eq!(*begin_account.key, end_account.pubkey);
                }
            } else {
                // ensure no one can cpi into mango either
                for meta in ix.accounts.iter() {
                    require_keys_neq!(meta.pubkey, crate::id());
                }
            }

            index += 1;
        }
        require!(found_end, MangoError::SomeError);
    }

    Ok(())
}

struct TokenVaultChange {
    bank_index: usize,
    raw_token_index: usize,
    amount: I80F48,
}

pub fn flash_loan3_end<'key, 'accounts, 'remaining, 'info>(
    ctx: Context<'key, 'accounts, 'remaining, 'info, FlashLoan3End<'info>>,
) -> Result<()> {
    let mut account = ctx.accounts.account.load_mut()?;
    require!(account.is_bankrupt == 0, MangoError::IsBankrupt);

    // Find index at which vaults start
    let vaults_index = ctx
        .remaining_accounts
        .iter()
        .position(|ai| {
            let maybe_token_account = Account::<TokenAccount>::try_from(ai);
            if maybe_token_account.is_err() {
                return false;
            }

            maybe_token_account.unwrap().owner == account.group
        })
        .ok_or_else(|| error!(MangoError::SomeError))?;
    let vaults_len = (ctx.remaining_accounts.len() - vaults_index) / 2;
    require_eq!(ctx.remaining_accounts.len(), vaults_index + 2 * vaults_len);

    // First initialize to the remaining delegated amount
    let health_ais = &ctx.remaining_accounts[..vaults_index];
    let vaults = &ctx.remaining_accounts[vaults_index..vaults_index + vaults_len];
    let token_accounts = &ctx.remaining_accounts[vaults_index + vaults_len..];
    let mut vaults_with_banks = vec![false; vaults.len()];

    // Loop over the banks, finding matching vaults
    // TODO: must be moved into health.rs, because it assumes something about the health accounts structure
    let mut changes = vec![];
    for (i, bank_ai) in health_ais.iter().enumerate() {
        // iterate until the first non-bank
        let bank = match bank_ai.load::<Bank>() {
            Ok(b) => b,
            Err(_) => break,
        };

        // find a vault -- if there's none, skip
        let (vault_index, vault_ai) = match vaults
            .iter()
            .enumerate()
            .find(|(_, vault_ai)| vault_ai.key == &bank.vault)
        {
            Some(v) => v,
            None => continue,
        };

        vaults_with_banks[vault_index] = true;
        let token_account_ai = &token_accounts[vault_index];
        let token_account = Account::<TokenAccount>::try_from(&token_account_ai)?;

        // Ensure this bank/vault combination was mentioned in the Begin instruction:
        // The Begin instruction only checks that End ends with the same vault accounts -
        // but there could be an extra vault account in End, or a different bank could be
        // used for the same vault.
        require_neq!(bank.flash_loan_vault_initial, u64::MAX);

        // Create the token position now, so we can compute the pre-health with fixed order health accounts
        let (_, raw_token_index) = account.tokens.get_mut_or_create(bank.token_index)?;

        // Transfer any excess over the inital balance of the token account back
        // into the vault. Compute the total change in the vault balance.
        let mut change = -I80F48::from(bank.flash_loan_approved_amount);
        if token_account.amount > bank.flash_loan_vault_initial {
            let transfer_ctx = CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token::Transfer {
                    from: token_account_ai.clone(),
                    to: vault_ai.clone(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            );
            let repay = token_account.amount - bank.flash_loan_vault_initial;
            token::transfer(transfer_ctx, repay)?;

            let repay = I80F48::from(repay);
            change = cm!(change + repay);
        }

        changes.push(TokenVaultChange {
            bank_index: i,
            raw_token_index,
            amount: change,
        });
    }

    // all vaults must have had matching banks
    require!(vaults_with_banks.iter().all(|&b| b), MangoError::SomeError);

    // Check pre-cpi health
    // NOTE: This health check isn't strictly necessary. It will be, later, when
    // we want to have reduce_only or be able to move an account out of bankruptcy.
    let pre_cpi_health =
        compute_health_from_fixed_accounts(&account, HealthType::Init, health_ais)?;
    require!(pre_cpi_health >= 0, MangoError::HealthMustBePositive);
    msg!("pre_cpi_health {:?}", pre_cpi_health);

    // Apply the vault diffs to the bank positions
    let mut deactivated_token_positions = vec![];
    for change in changes {
        let mut bank = health_ais[change.bank_index].load_mut::<Bank>()?;
        let position = account.tokens.get_mut_raw(change.raw_token_index);
        let native = position.native(&bank);
        let approved_amount = I80F48::from(bank.flash_loan_approved_amount);

        let loan = if native.is_positive() {
            cm!(approved_amount - native).max(I80F48::ZERO)
        } else {
            approved_amount
        };

        let loan_origination_fee = cm!(loan * bank.loan_origination_fee_rate);
        bank.collected_fees_native = cm!(bank.collected_fees_native + loan_origination_fee);

        let is_active =
            bank.change_without_fee(position, cm!(change.amount - loan_origination_fee))?;
        if !is_active {
            deactivated_token_positions.push(change.raw_token_index);
        }

        bank.flash_loan_approved_amount = 0;
        bank.flash_loan_vault_initial = u64::MAX;
    }

    // Check post-cpi health
    let post_cpi_health =
        compute_health_from_fixed_accounts(&account, HealthType::Init, health_ais)?;
    require!(post_cpi_health >= 0, MangoError::HealthMustBePositive);
    msg!("post_cpi_health {:?}", post_cpi_health);

    // Deactivate inactive token accounts after health check
    for raw_token_index in deactivated_token_positions {
        account.tokens.deactivate(raw_token_index);
    }

    Ok(())
}