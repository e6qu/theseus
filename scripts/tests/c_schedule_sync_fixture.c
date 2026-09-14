#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int value;

__attribute__((noinline)) static void hold_lock_at_a_schedule_point(void) {
    __asm__ volatile("" ::: "memory");
}

static void *reader(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    while (value == 0) {
        pthread_cond_wait(&changed, &lock);
    }
    printf("value=%d\n", value);
    pthread_mutex_unlock(&lock);
    return NULL;
}

static void *writer(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    hold_lock_at_a_schedule_point();
    value = 42;
    pthread_cond_signal(&changed);
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);
    return NULL;
}

int main(void) {
    pthread_t reading;
    pthread_t writing;
    if (pthread_create(&reading, NULL, reader, NULL) != 0 ||
        pthread_create(&writing, NULL, writer, NULL) != 0) {
        return 2;
    }
    pthread_join(reading, NULL);
    pthread_join(writing, NULL);
    return value == 42 ? 0 : 1;
}
