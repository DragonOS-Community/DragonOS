#pragma once

#ifndef __ASM__
#define __ASM__


#define ENTRY(name)\
    .global name;    \
    name:


#endif