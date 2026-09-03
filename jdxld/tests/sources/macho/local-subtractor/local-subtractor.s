//#Arch:aarch64
//#RunEnabled:false

.data
.globl _difference
_difference:
    .quad .Ldata - .Ltext
.Ldata:
    .quad 0

.text
.globl _main
_main:
.Ltext:
    ret
