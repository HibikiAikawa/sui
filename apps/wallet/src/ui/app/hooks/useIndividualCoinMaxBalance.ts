import { useMemo } from 'react';
import { accountItemizedBalancesSelector } from '_redux/slices/account';
import useAppSelector from './useAppSelector';

export function useIndividualCoinMaxBalance(coinTypeArg: string) {
    const allCoins = useAppSelector(accountItemizedBalancesSelector);
    const maxGasCoinBalance = useMemo(
        () =>
            allCoins[coinTypeArg]?.reduce(
                (max, aBalance) => (max < aBalance ? aBalance : max),
                BigInt(0)
            ) || BigInt(0),
        [allCoins]
    );
    return maxGasCoinBalance;
}
