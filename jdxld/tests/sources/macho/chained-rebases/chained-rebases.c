//#Object:runtime.c
//#ReferenceLinkers:ld
//#DiffIgnore:section.__unwind_info

#include "../common/runtime.h"

static int first = 20;
static int second = 22;

volatile int *first_pointer = &first;
volatile int *second_pointer __attribute__((aligned(16384))) = &second;

void main(void) { exit_syscall(*first_pointer + *second_pointer); }
