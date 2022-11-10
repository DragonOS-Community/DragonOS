#include "ia64_msi.h"

/**
 * @brief 生成架构相关的msi的message address
 *
 */
#define ia64_pci_get_arch_msi_message_address(processor) ((0xfee00000UL | (processor << 12)))

/**
 * @brief 生成架构相关的message data
 *
 */
#define ia64_pci_get_arch_msi_message_data(vector, processor, edge_trigger, assert) ((uint32_t)((vector & 0xff) | (edge_trigger == 1 ? 0 : (1 << 15)) | ((assert == 0) ? 0 : (1 << 14))))

/**
 * @brief 生成msi消息
 *
 * @param msi_desc msi描述符
 * @return struct msi_msg_t* msi消息指针（在描述符内）
 */
struct msi_msg_t *msi_arch_get_msg(struct msi_desc_t *msi_desc)
{
    msi_desc->msg.address_hi = 0;
    msi_desc->msg.address_lo = ia64_pci_get_arch_msi_message_address(msi_desc->processor);
    msi_desc->msg.data = ia64_pci_get_arch_msi_message_data(msi_desc->irq_num, msi_desc->processor, msi_desc->edge_trigger, msi_desc->assert);
    msi_desc->msg.vector_control = 0;
    return &(msi_desc->msg);
}
