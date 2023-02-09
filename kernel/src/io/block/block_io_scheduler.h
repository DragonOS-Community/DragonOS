#pragma once

extern void block_io_scheduler_address_requests();
extern void block_io_scheduler_init_rust();

/**
 * @brief 初始化io调度器
 */
void block_io_scheduler_init();
