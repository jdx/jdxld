//#Object:runtime.c
//#ReferenceLinkers:ld
//#ExpectSection:__thread_data
//#ExpectSection:__thread_bss
//#ExpectSection:__thread_vars
//#DiffIgnore:section.__unwind_info

#include "../common/runtime.h"

_Thread_local int initialized = 1;
_Thread_local int uninitialized __attribute__((aligned(64)));

void main(void) {
  uninitialized = 41;
  exit_syscall(initialized + uninitialized);
}
