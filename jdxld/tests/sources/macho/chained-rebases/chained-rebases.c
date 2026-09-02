//#Object:runtime.c
//#ReferenceLinkers:ld

#include "../common/runtime.h"

static int first = 20;
static int second = 22;

volatile int *first_pointer = &first;
__attribute__((aligned(16384))) volatile int *second_pointer = &second;

void main(void) { exit_syscall(*first_pointer + *second_pointer); }
