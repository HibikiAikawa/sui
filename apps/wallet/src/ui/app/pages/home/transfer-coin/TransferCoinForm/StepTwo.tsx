// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { Coin } from '@mysten/sui.js';
import { useQuery } from '@tanstack/react-query';
import BigNumber from 'bignumber.js';
import cl from 'classnames';
import { Field, Form, useFormikContext } from 'formik';
import { useEffect, useRef, memo, useMemo } from 'react';

import { parseAmount } from './utils';
import { Content, Menu } from '_app/shared/bottom-menu-layout';
import Button from '_app/shared/button';
import AddressInput from '_components/address-input';
import Icon, { SuiIcons } from '_components/icon';
import LoadingIndicator from '_components/loading/LoadingIndicator';
import {
    useAppSelector,
    useCoinDecimals,
    useFormatCoin,
    useSigner,
} from '_hooks';
import { GAS_SYMBOL, GAS_TYPE_ARG } from '_redux/slices/sui-objects/Coin';
import { accountCoinsSelector } from '_src/ui/app/redux/slices/account';

import type { FormValues } from '../';

import st from './TransferCoinForm.module.scss';

export type TransferCoinFormProps = {
    submitError: string | null;
    coinSymbol: string;
    coinType: string;
    gasBudget: number;
    onClearSubmitError: () => void;
};

function StepTwo({
    submitError,
    coinSymbol,
    coinType,
    gasBudget,
    onClearSubmitError,
}: TransferCoinFormProps) {
    const {
        isSubmitting,
        isValid,
        isValidating,
        values: { amount, to },
    } = useFormikContext<FormValues>();

    const onClearRef = useRef(onClearSubmitError);
    onClearRef.current = onClearSubmitError;

    useEffect(() => {
        onClearRef.current();
    }, [amount, to]);

    const [decimals] = useCoinDecimals(coinType);
    const amountWithoutDecimals = useMemo(
        () =>
            new BigNumber(amount).shiftedBy(decimals).integerValue().toString(),
        [amount, decimals]
    );
    const allCoins = useAppSelector(accountCoinsSelector);
    const signer = useSigner();
    const isGasEstimationQueryEnabled = !!(
        isValid &&
        !isValidating &&
        amount &&
        to
    );
    const gasEstimationResult = useQuery({
        queryKey: ['token-transfer', coinType, amount, to],
        queryFn: async () => {
            if (isGasEstimationQueryEnabled) {
                return await Coin.estimateTransferGasCost(
                    signer,
                    allCoins,
                    coinType,
                    parseAmount(amount, decimals),
                    to
                );
            }
            return null;
        },
        enabled: isGasEstimationQueryEnabled,
    });
    const gasEstimation = gasEstimationResult.isError
        ? gasBudget
        : gasEstimationResult.data ?? null;
    const totalAmount = new BigNumber(gasEstimation || 0)
        .plus(GAS_SYMBOL === coinSymbol ? amountWithoutDecimals : 0)
        .toString();
    console.log({
        gasEstimation,
        gasBudget,
        totalAmount,
        error: gasEstimationResult.error,
    });
    const validAddressBtn = !isValid || to === '' || isSubmitting;

    const [formattedBalance] = useFormatCoin(amountWithoutDecimals, coinType);
    const [formattedTotal] = useFormatCoin(totalAmount, GAS_TYPE_ARG);
    const [formattedGas] = useFormatCoin(gasEstimation, GAS_TYPE_ARG);

    return (
        <Form className={st.container} autoComplete="off" noValidate={true}>
            <Content>
                <div className={st.labelDirection}>
                    Enter or search the address of the recepient below to start
                    sending coins.
                </div>
                <div className={cl(st.group, st.address)}>
                    <Field
                        component={AddressInput}
                        name="to"
                        className={st.input}
                    />
                </div>

                {submitError ? (
                    <div className={st.error}>{submitError}</div>
                ) : null}

                <div className={st.responseCard}>
                    <div className={st.amount}>
                        {formattedBalance} <span>{coinSymbol}</span>
                    </div>

                    <div className={st.details}>
                        {[
                            ['Estimated Gas Fee', formattedGas, GAS_SYMBOL],
                            ['Total Amount', formattedTotal, GAS_SYMBOL],
                        ].map(([label, frmt, symbol]) => (
                            <div className={st.txFees} key={label}>
                                <div className={st.txInfoLabel}>{label}</div>
                                <div className={st.walletInfoValue}>
                                    {isGasEstimationQueryEnabled &&
                                    gasEstimationResult.isLoading ? (
                                        <LoadingIndicator />
                                    ) : frmt ? (
                                        `${frmt} ${symbol}`
                                    ) : (
                                        '-'
                                    )}
                                </div>
                            </div>
                        ))}
                    </div>
                </div>
            </Content>
            <Menu stuckClass={st.shadow}>
                <div className={cl(st.group, st.cta)}>
                    <Button
                        type="submit"
                        disabled={validAddressBtn}
                        mode="primary"
                        className={st.btn}
                    >
                        {isSubmitting ||
                        (isGasEstimationQueryEnabled &&
                            gasEstimationResult.isLoading) ? (
                            <LoadingIndicator />
                        ) : (
                            'Send Coins Now'
                        )}
                        <Icon
                            icon={SuiIcons.ArrowLeft}
                            className={cl(st.arrowLeft)}
                        />
                    </Button>
                </div>
            </Menu>
        </Form>
    );
}

export default memo(StepTwo);
