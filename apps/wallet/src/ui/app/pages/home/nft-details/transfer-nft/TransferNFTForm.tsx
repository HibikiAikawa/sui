// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { SUI_TYPE_ARG } from '@mysten/sui.js';
import cl from 'classnames';
import { Form, Field, useFormikContext } from 'formik';
import { useEffect, useRef, memo, useMemo } from 'react';

import { Content } from '_app/shared/bottom-menu-layout';
import Button from '_app/shared/button';
import AddressInput from '_components/address-input';
import Icon, { SuiIcons } from '_components/icon';
import LoadingIndicator from '_components/loading/LoadingIndicator';
import { useIndividualCoinMaxBalance } from '_src/ui/app/hooks/useIndividualCoinMaxBalance';

import type { FormValues } from '.';

import st from './TransferNFTForm.module.scss';

export type TransferNFTFormProps = {
    submitError: string | null;
    gasBudget: number | null;
    isGasEstimationLoading: boolean;
    onClearSubmitError: () => void;
};

function TransferNFTForm({
    submitError,
    gasBudget,
    isGasEstimationLoading,
    onClearSubmitError,
}: TransferNFTFormProps) {
    const {
        isSubmitting,
        isValid,
        values: { to },
    } = useFormikContext<FormValues>();
    const onClearRef = useRef(onClearSubmitError);
    onClearRef.current = onClearSubmitError;
    useEffect(() => {
        onClearRef.current();
    }, [to]);
    const maxGasCoinBalance = useIndividualCoinMaxBalance(SUI_TYPE_ARG);
    const isInsufficientGas =
        gasBudget !== null ? maxGasCoinBalance < BigInt(gasBudget) : null;
    return (
        <div className={st.sendNft}>
            <Content>
                <Form
                    className={st.container}
                    autoComplete="off"
                    noValidate={true}
                >
                    <label className={st.labelInfo}>
                        Enter the address of the recipient to start sending the
                        NFT
                    </label>
                    <div className={st.group}>
                        <Field
                            component={AddressInput}
                            name="to"
                            as="div"
                            id="to"
                            placeholder="Enter Address"
                            className={st.input}
                        />
                    </div>
                    {isInsufficientGas ? (
                        <div className={st.error}>
                            * Insufficient balance to cover transfer cost
                        </div>
                    ) : null}
                    {submitError ? (
                        <div className={st.error}>{submitError}</div>
                    ) : null}
                    <div className={st.formcta}>
                        <Button
                            size="large"
                            mode="primary"
                            type="submit"
                            disabled={
                                !isValid ||
                                isSubmitting ||
                                isInsufficientGas ||
                                isGasEstimationLoading ||
                                gasBudget === null
                            }
                            className={cl(st.action, 'btn', st.sendNftBtn)}
                        >
                            {isSubmitting || isGasEstimationLoading ? (
                                <LoadingIndicator />
                            ) : (
                                <>
                                    Send NFT Now
                                    <Icon
                                        icon={SuiIcons.ArrowRight}
                                        className={st.arrowActionIcon}
                                    />
                                </>
                            )}
                        </Button>
                    </div>
                </Form>
            </Content>
        </div>
    );
}

export default memo(TransferNFTForm);
