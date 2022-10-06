#pragma once

/*
    注意！！！

    尽管采用MMI/O的方式访问寄存器，但是对于指定大小的寄存器，
    在发起读请求的时候，只能从寄存器的起始地址位置开始读取。

    例子：不能在一个32bit的寄存器中的偏移量8的位置开始读取1个字节
            这种情况下，我们必须从32bit的寄存器的0地址处开始读取32bit，然后通过移位的方式得到其中的字节。
*/

#define xhci_read_cap_reg32(id, offset) (__read4b(xhci_hc[id].vbase + (offset)))
#define xhci_get_ptr_cap_reg32(id, offset) ((uint32_t *)(xhci_hc[id].vbase + (offset)))
#define xhci_write_cap_reg32(id, offset, value) (__write4b(xhci_hc[id].vbase + (offset), (value)))

#define xhci_read_cap_reg64(id, offset) (__read8b(xhci_hc[id].vbase + (offset)))
#define xhci_get_ptr_reg64(id, offset) ((uint64_t *)(xhci_hc[id].vbase + (offset)))
#define xhci_write_cap_reg64(id, offset, value) (__write8b(xhci_hc[id].vbase + (offset), (value)))

#define xhci_read_op_reg8(id, offset) (*(uint8_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_get_ptr_op_reg8(id, offset) ((uint8_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_write_op_reg8(id, offset, value) (*(uint8_t *)(xhci_hc[id].vbase_op + (offset)) = (uint8_t)(value))

#define xhci_read_op_reg32(id, offset) (__read4b(xhci_hc[id].vbase_op + (offset)))
#define xhci_get_ptr_op_reg32(id, offset) ((uint32_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_write_op_reg32(id, offset, value) (__write4b(xhci_hc[id].vbase_op + (offset), (value)))

#define xhci_read_op_reg64(id, offset) (__read8b(xhci_hc[id].vbase_op + (offset)))
#define xhci_get_ptr_op_reg64(id, offset) ((uint64_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_write_op_reg64(id, offset, value) (__write8b(xhci_hc[id].vbase_op + (offset), (value)))

/**
 * @brief 计算中断寄存器组虚拟地址
 * @param id 主机控制器id
 * @param num xhci中断寄存器组号
 */
#define xhci_calc_intr_vaddr(id, num) (xhci_hc[id].vbase + xhci_hc[id].rts_offset + XHCI_RT_IR0 + (num)*XHCI_IR_SIZE)
/**
 * @brief 读取/写入中断寄存器
 * @param id 主机控制器id
 * @param num xhci中断寄存器组号
 * @param intr_offset 寄存器在当前寄存器组中的偏移量
 */
#define xhci_read_intr_reg32(id, num, intr_offset) (__read4b(xhci_calc_intr_vaddr(id, num) + (intr_offset)))
#define xhci_write_intr_reg32(id, num, intr_offset, value) (__write4b(xhci_calc_intr_vaddr(id, num) + (intr_offset), (value)))
#define xhci_read_intr_reg64(id, num, intr_offset) (__read8b(xhci_calc_intr_vaddr(id, num) + (intr_offset)))
#define xhci_write_intr_reg64(id, num, intr_offset, value) (__write8b(xhci_calc_intr_vaddr(id, num) + (intr_offset), (value)))

#define xhci_is_aligned64(addr) (((addr)&0x3f) == 0) // 是否64bytes对齐

/**
 * @brief 判断端口信息
 * @param cid 主机控制器id
 * @param pid 端口id
 */
#define XHCI_PORT_IS_USB2(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_INFO) == XHCI_PROTOCOL_USB2)
#define XHCI_PORT_IS_USB3(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_INFO) == XHCI_PROTOCOL_USB3)

#define XHCI_PORT_IS_USB2_HSO(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_HSO) == XHCI_PROTOCOL_HSO)
#define XHCI_PORT_HAS_PAIR(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_HAS_PAIR) == XHCI_PROTOCOL_HAS_PAIR)
#define XHCI_PORT_IS_ACTIVE(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_ACTIVE) == XHCI_PROTOCOL_ACTIVE)

#define XHCI_PORT_REGISTER_OFFSET(__port_id) (XHCI_OPS_PRS + 16 * (__port_id))

// 获取端口速度 full=1, low=2, high=3, super=4
#define xhci_get_port_speed(__id, __port_id) ((xhci_read_op_reg32((__id), XHCI_PORT_REGISTER_OFFSET(__port_id) + XHCI_PORT_PORTSC) >> 10) & 0xf)

/**
 * @brief 设置link TRB的命令（dword3）
 *
 */
#define xhci_TRB_set_link_cmd(trb_vaddr)                                         \
    do                                                                           \
    {                                                                            \
        struct xhci_TRB_normal_t *ptr = (struct xhci_TRB_normal_t *)(trb_vaddr); \
        ptr->TRB_type = TRB_TYPE_LINK;                                           \
        ptr->ioc = 0;                                                            \
        ptr->chain = 0;                                                          \
        ptr->ent = 0;                                                            \
        ptr->cycle = 1;                                                          \
    } while (0)

// 设置endpoint结构体的dequeue_cycle_state bit
#define xhci_ep_set_dequeue_cycle_state(ep_ctx_ptr, state) ((ep_ctx_ptr)->tr_dequeue_ptr |= ((state)&1))
// 获取endpoint结构体的dequeue_cycle_state bit
#define xhci_ep_get_dequeue_cycle_state(ep_ctx_ptr) (((ep_ctx_ptr)->tr_dequeue_ptr) & 1)