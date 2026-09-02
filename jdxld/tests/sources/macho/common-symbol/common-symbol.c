//#Object:runtime.c
//#ReferenceLinkers:ld
//#ExpectSection:__common

#include "../common/runtime.h"

int value __attribute__((common));

void main(void) {
    value = 42;
    exit_syscall(value);
}
