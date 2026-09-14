#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>

static atomic_int balance;

__attribute__((noinline)) static int read_balance(void) {
    return atomic_load_explicit(&balance, memory_order_relaxed);
}

__attribute__((noinline)) static void write_balance(int value) {
    atomic_store_explicit(&balance, value, memory_order_relaxed);
}

static void *deposit(void *amount) {
    int current = read_balance();
    write_balance(current + *(int *)amount);
    return NULL;
}

int main(void) {
    int first = 20;
    int second = 22;
    pthread_t one;
    pthread_t two;
    if (pthread_create(&one, NULL, deposit, &first) != 0 ||
        pthread_create(&two, NULL, deposit, &second) != 0) {
        return 2;
    }
    pthread_join(one, NULL);
    pthread_join(two, NULL);
    printf("{\"balance\":%d}\n", atomic_load(&balance));
    return 0;
}
