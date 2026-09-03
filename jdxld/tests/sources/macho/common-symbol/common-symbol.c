//#Object:runtime.c
//#ReferenceLinkers:ld
//#ExpectSection:__common
//#DiffIgnore:section.__unwind_info

#include "../common/runtime.h"

int value __attribute__((common));

void main(void) {
  value = 42;
  exit_syscall(value);
}
