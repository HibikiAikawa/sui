import { getTotalGasUsed, SUI_TYPE_ARG } from '@mysten/sui.js';
import { useQuery } from '@tanstack/react-query';

import { useSigner, useIndividualCoinMaxBalance } from '_hooks';

import type { ObjectId } from '@mysten/sui.js';

const DEFAULT_NFT_TRANSFER_GAS_FEE = 450;

export function useGasEstimation(objectId: ObjectId) {
    const suiCoinMaxBalance = useIndividualCoinMaxBalance(SUI_TYPE_ARG);
    console.log('maxSuiCoinBalance', suiCoinMaxBalance);
    const signer = useSigner();
    const estimationResult = useQuery({
        queryKey: ['nft-transfer', 'gas-estimation', objectId],
        queryFn: async () => {
            try {
                const address = await signer.getAddress();
                console.log(address);
                const localGasBudgetGuess = Math.min(
                    DEFAULT_NFT_TRANSFER_GAS_FEE,
                    Number(suiCoinMaxBalance)
                );
                const tx = await signer.serializer.newTransferObject(address, {
                    objectId: objectId,
                    recipient: address, // gas cost is the same regardless the recipient
                    gasBudget: localGasBudgetGuess,
                });
                const result = await signer.dryRunTransaction(tx);
                const isSuccess = result.status.status === 'success';
                const cost =
                    (isSuccess ? getTotalGasUsed(result) : null) ?? null;
                const budget = isSuccess
                    ? result.gasUsed.computationCost +
                      result.gasUsed.storageCost
                    : null;
                console.log(result, cost, isSuccess, budget);
                return {
                    budget,
                    cost,
                    insufficientGas: result.status.error === 'InsufficientGas',
                };
            } catch (e) {
                // use default in case of errors (network error, not supported, etc.)
                return {
                    budget: DEFAULT_NFT_TRANSFER_GAS_FEE,
                    cost: DEFAULT_NFT_TRANSFER_GAS_FEE,
                };
            }
        },
        enabled: !!objectId, // just in case
    });
    return [
        estimationResult.data?.budget ?? null,
        estimationResult.data?.cost ?? null,
        estimationResult.isLoading,
        estimationResult.data?.insufficientGas || false,
    ] as const;
}
