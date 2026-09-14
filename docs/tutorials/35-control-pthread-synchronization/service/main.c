#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int value;
static int readers_finished;

__attribute__((noinline)) static void hold_lock_at_a_schedule_point(void) {
    __asm__ volatile("" ::: "memory");
}

static void *reader(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    while (value == 0) {
        pthread_cond_wait(&changed, &lock);
    }
    readers_finished++;
    pthread_mutex_unlock(&lock);
    return NULL;
}

static void *writer(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    hold_lock_at_a_schedule_point();
    value = 42;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);
    return NULL;
}

int main(void) {
    pthread_t first_reader;
    pthread_t second_reader;
    pthread_t writing;
    if (pthread_create(&first_reader, NULL, reader, NULL) != 0 ||
        pthread_create(&second_reader, NULL, reader, NULL) != 0 ||
        pthread_create(&writing, NULL, writer, NULL) != 0) {
        return 2;
    }
    pthread_join(first_reader, NULL);
    pthread_join(second_reader, NULL);
    pthread_join(writing, NULL);
    printf("{\"value\":%d,\"readers\":%d}\n", value, readers_finished);
    return value == 42 && readers_finished == 2 ? 0 : 1;
}
