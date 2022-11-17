// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { Coin, SUI_TYPE_ARG, getTransactionDigest } from '@mysten/sui.js';
import BigNumber from 'bignumber.js';
import { Formik } from 'formik';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Navigate, useNavigate, useSearchParams } from 'react-router-dom';
import { v4 as uuidV4 } from 'uuid';

import StepOne from './TransferCoinForm/StepOne';
import StepTwo from './TransferCoinForm/StepTwo';
import { parseAmount } from './TransferCoinForm/utils';
import {
    createValidationSchemaStepOne,
    createValidationSchemaStepTwo,
} from './validation';
import { Content } from '_app/shared/bottom-menu-layout';
import PageTitle from '_app/shared/page-title';
import Loading from '_components/loading';
import ProgressBar from '_components/progress-bar';
import { useAppSelector, useAppDispatch, useCoinDecimals } from '_hooks';
import {
    accountAggregateBalancesSelector,
    accountCoinsSelector,
} from '_redux/slices/account';
import { sendTokens } from '_redux/slices/transactions';
import { trackEvent } from '_src/shared/plausible';

import type { SerializedError } from '@reduxjs/toolkit';
import type { FormikHelpers } from 'formik';

import st from './TransferCoinPage.module.scss';

const initialValues = {
    to: '',
    amount: '',
};

export type FormValues = typeof initialValues;

const DEFAULT_FORM_STEP = 1;

// TODO: show out of sync when sui objects locally might be outdated
function TransferCoinPage() {
    const [searchParams] = useSearchParams();
    const coinType = searchParams.get('type');

    const aggregateBalances = useAppSelector(accountAggregateBalancesSelector);
    const coinBalance = useMemo(
        () => (coinType && aggregateBalances[coinType]) || BigInt(0),
        [coinType, aggregateBalances]
    );

    const gasAggregateBalance = useMemo(
        () => aggregateBalances[SUI_TYPE_ARG] || BigInt(0),
        [aggregateBalances]
    );

    const coinSymbol = useMemo(
        () => (coinType && Coin.getCoinSymbol(coinType)) || '',
        [coinType]
    );

    const [sendError, setSendError] = useState<string | null>(null);
    const [currentStep, setCurrentStep] = useState<number>(DEFAULT_FORM_STEP);
    const [formData] = useState<FormValues>(initialValues);

    const [coinDecimals] = useCoinDecimals(coinType);
    const [gasDecimals] = useCoinDecimals(SUI_TYPE_ARG);
    const allCoins = useAppSelector(accountCoinsSelector);
    const allCoinsOfSelectedTypeArg = useMemo(
        () =>
            allCoins.filter(
                (aCoin) => coinType && Coin.getCoinTypeArg(aCoin) === coinType
            ),
        [coinType, allCoins]
    );
    const [amountToSend, setAmountToSend] = useState(BigInt(0));
    const gasBudget = useMemo(
        () =>
            Coin.computeTransferTxGasBudget(
                allCoinsOfSelectedTypeArg,
                amountToSend
            ),
        [allCoinsOfSelectedTypeArg, amountToSend]
    );
    const validationSchemaStepOne = useMemo(
        () =>
            createValidationSchemaStepOne(
                coinType || '',
                coinBalance,
                coinSymbol,
                gasAggregateBalance,
                coinDecimals,
                gasDecimals,
                gasBudget
            ),
        [
            coinType,
            coinBalance,
            coinSymbol,
            coinDecimals,
            gasDecimals,
            gasAggregateBalance,
            gasBudget,
        ]
    );
    const validationSchemaStepTwo = useMemo(
        () => createValidationSchemaStepTwo(),
        []
    );

    const dispatch = useAppDispatch();
    const navigate = useNavigate();
    const onHandleSubmit = useCallback(
        async (
            { to, amount }: FormValues,
            { resetForm }: FormikHelpers<FormValues>
        ) => {
            if (coinType === null || !gasBudget) {
                return;
            }
            setSendError(null);
            trackEvent('TransferCoins', {
                props: { coinType },
            });
            try {
                const bigIntAmount = parseAmount(amount, coinDecimals);
                const response = await dispatch(
                    sendTokens({
                        amount: bigIntAmount,
                        recipientAddress: to,
                        tokenTypeArg: coinType,
                        gasBudget,
                    })
                ).unwrap();

                resetForm();
                const txDigest = getTransactionDigest(response);
                const receiptUrl = `/receipt?txdigest=${encodeURIComponent(
                    txDigest
                )}&transfer=coin`;

                navigate(receiptUrl);
            } catch (e) {
                setSendError((e as SerializedError).message || null);
            }
        },
        [dispatch, navigate, coinType, coinDecimals]
    );

    const handleNextStep = useCallback(
        (_: FormValues, { setSubmitting }: FormikHelpers<FormValues>) => {
            setCurrentStep((prev) => prev + 1);
            setSubmitting(false);
        },
        []
    );

    const handleBackStep = useCallback(() => {
        setCurrentStep(DEFAULT_FORM_STEP);
    }, []);

    const handleOnClearSubmitError = useCallback(() => {
        setSendError(null);
    }, []);
    const loadingBalance = useAppSelector(
        ({ suiObjects }) => suiObjects.loading && !suiObjects.lastSync
    );

    if (!coinType) {
        return <Navigate to="/" replace={true} />;
    }

    const StepOneForm = (
        <Formik
            initialValues={formData}
            validateOnMount={true}
            validationSchema={validationSchemaStepOne}
            onSubmit={handleNextStep}
        >
            <StepOne
                coinSymbol={coinSymbol}
                coinType={coinType}
                onClearSubmitError={handleOnClearSubmitError}
                onAmountChanged={(anAmount) => setAmountToSend(anAmount)}
            />
        </Formik>
    );

    const StepTwoForm = (
        <Formik
            initialValues={formData}
            validateOnMount={true}
            validationSchema={validationSchemaStepTwo}
            onSubmit={onHandleSubmit}
        >
            <StepTwo
                submitError={sendError}
                coinSymbol={coinSymbol}
                coinType={coinType}
                gasBudget={gasBudget}
                onClearSubmitError={handleOnClearSubmitError}
            />
        </Formik>
    );

    const steps = [StepOneForm, StepTwoForm];

    const SendCoin = (
        <div className={st.container}>
            <PageTitle
                title="Send Coins"
                backLink={'/'}
                className={st.pageTitle}
                {...(currentStep > 1 && { onClick: handleBackStep })}
            />

            <Content className={st.content}>
                <Loading loading={loadingBalance}>
                    <ProgressBar
                        currentStep={currentStep}
                        stepsName={['Amount', 'Address']}
                    />
                    {steps[currentStep - 1]}
                </Loading>
            </Content>
        </div>
    );

    return <>{SendCoin}</>;
}

export default TransferCoinPage;
