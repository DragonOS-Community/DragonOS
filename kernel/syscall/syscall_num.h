#pragma once

/**
 * 系统调用说明
 * 1    printf
 * 
 * 
 * 255  AHCI end_request
 * 
 */

#define SYS_NOT_EXISTS 0
#define SYS_PUT_STRING 1
#define SYS_OPEN 2

#define SYS_AHCI_END_REQ 255    // AHCI DMA请求结束end_request的系统调用