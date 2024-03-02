use crate::{
    arch::MMArch,
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
};

extern "C" {
    static mut IDT_Table: [usize; 0usize];
}
macro_rules! save_all_regs {
    () => {
        "
        cld
        push rax
        push rax
        mov rax, es
        push rax
        mov rax, ds
        push rax
        xor rax, rax
        push rbp
        push rdi
        push rsi
        push rdx
        push rcx
        push rbx
        push r8
        push r9
        push r10
        push r11
        push r12
        push r13
        push r14
        push r15
        mov rdx, 0x10
        mov ds, rdx
        mov es, rdx
        "
    };
}

macro_rules! interrupt_handler {
    ($name:expr) => {
        paste::paste! {

            #[naked]
            #[no_mangle]
            unsafe extern "C" fn [<irq_handler $name>]() {
                core::arch::asm!(
                    concat!(
                        "
                        push 0x0
                        ",
                        save_all_regs!(),
                        "\n",
                        "
                        mov rdi, rsp
                        lea rax, ret_from_intr[rip]
                        push rax
                        mov rsi, {irqnum}
                        jmp x86_64_do_irq
                        // jmp do_IRQ
                        "
                    ),
                    irqnum = const($name),
                    options(noreturn)
                );
            }
        }
    };
}

interrupt_handler!(32);
interrupt_handler!(33);
interrupt_handler!(34);
interrupt_handler!(35);
interrupt_handler!(36);
interrupt_handler!(37);
interrupt_handler!(38);
interrupt_handler!(39);
interrupt_handler!(40);
interrupt_handler!(41);
interrupt_handler!(42);
interrupt_handler!(43);
interrupt_handler!(44);
interrupt_handler!(45);
interrupt_handler!(46);
interrupt_handler!(47);
interrupt_handler!(48);
interrupt_handler!(49);
interrupt_handler!(50);
interrupt_handler!(51);
interrupt_handler!(52);
interrupt_handler!(53);
interrupt_handler!(54);
interrupt_handler!(55);
interrupt_handler!(56);
interrupt_handler!(57);
interrupt_handler!(58);
interrupt_handler!(59);
interrupt_handler!(60);
interrupt_handler!(61);
interrupt_handler!(62);
interrupt_handler!(63);
interrupt_handler!(64);
interrupt_handler!(65);
interrupt_handler!(66);
interrupt_handler!(67);
interrupt_handler!(68);
interrupt_handler!(69);
interrupt_handler!(70);
interrupt_handler!(71);
interrupt_handler!(72);
interrupt_handler!(73);
interrupt_handler!(74);
interrupt_handler!(75);
interrupt_handler!(76);
interrupt_handler!(77);
interrupt_handler!(78);
interrupt_handler!(79);
interrupt_handler!(80);
interrupt_handler!(81);
interrupt_handler!(82);
interrupt_handler!(83);
interrupt_handler!(84);
interrupt_handler!(85);
interrupt_handler!(86);
interrupt_handler!(87);
interrupt_handler!(88);
interrupt_handler!(89);
interrupt_handler!(90);
interrupt_handler!(91);
interrupt_handler!(92);
interrupt_handler!(93);
interrupt_handler!(94);
interrupt_handler!(95);
interrupt_handler!(96);
interrupt_handler!(97);
interrupt_handler!(98);
interrupt_handler!(99);
interrupt_handler!(100);
interrupt_handler!(101);
interrupt_handler!(102);
interrupt_handler!(103);
interrupt_handler!(104);
interrupt_handler!(105);
interrupt_handler!(106);
interrupt_handler!(107);
interrupt_handler!(108);
interrupt_handler!(109);
interrupt_handler!(110);
interrupt_handler!(111);
interrupt_handler!(112);
interrupt_handler!(113);
interrupt_handler!(114);
interrupt_handler!(115);
interrupt_handler!(116);
interrupt_handler!(117);
interrupt_handler!(118);
interrupt_handler!(119);
interrupt_handler!(120);
interrupt_handler!(121);
interrupt_handler!(122);
interrupt_handler!(123);
interrupt_handler!(124);
interrupt_handler!(125);
interrupt_handler!(126);
interrupt_handler!(127);
// 128号为系统调用，因此不需要设置中断处理函数
interrupt_handler!(129);
interrupt_handler!(130);
interrupt_handler!(131);
interrupt_handler!(132);
interrupt_handler!(133);
interrupt_handler!(134);
interrupt_handler!(135);
interrupt_handler!(136);
interrupt_handler!(137);
interrupt_handler!(138);
interrupt_handler!(139);
interrupt_handler!(140);
interrupt_handler!(141);
interrupt_handler!(142);
interrupt_handler!(143);
interrupt_handler!(144);
interrupt_handler!(145);
interrupt_handler!(146);
interrupt_handler!(147);
interrupt_handler!(148);
interrupt_handler!(149);
interrupt_handler!(150);
interrupt_handler!(151);
interrupt_handler!(152);
interrupt_handler!(153);
interrupt_handler!(154);
interrupt_handler!(155);
interrupt_handler!(156);
interrupt_handler!(157);
interrupt_handler!(158);
interrupt_handler!(159);
interrupt_handler!(160);
interrupt_handler!(161);
interrupt_handler!(162);
interrupt_handler!(163);
interrupt_handler!(164);
interrupt_handler!(165);
interrupt_handler!(166);
interrupt_handler!(167);
interrupt_handler!(168);
interrupt_handler!(169);
interrupt_handler!(170);
interrupt_handler!(171);
interrupt_handler!(172);
interrupt_handler!(173);
interrupt_handler!(174);
interrupt_handler!(175);
interrupt_handler!(176);
interrupt_handler!(177);
interrupt_handler!(178);
interrupt_handler!(179);
interrupt_handler!(180);
interrupt_handler!(181);
interrupt_handler!(182);
interrupt_handler!(183);
interrupt_handler!(184);
interrupt_handler!(185);
interrupt_handler!(186);
interrupt_handler!(187);
interrupt_handler!(188);
interrupt_handler!(189);
interrupt_handler!(190);
interrupt_handler!(191);
interrupt_handler!(192);
interrupt_handler!(193);
interrupt_handler!(194);
interrupt_handler!(195);
interrupt_handler!(196);
interrupt_handler!(197);
interrupt_handler!(198);
interrupt_handler!(199);
interrupt_handler!(200);
interrupt_handler!(201);
interrupt_handler!(202);
interrupt_handler!(203);
interrupt_handler!(204);
interrupt_handler!(205);
interrupt_handler!(206);
interrupt_handler!(207);
interrupt_handler!(208);
interrupt_handler!(209);
interrupt_handler!(210);
interrupt_handler!(211);
interrupt_handler!(212);
interrupt_handler!(213);
interrupt_handler!(214);
interrupt_handler!(215);
interrupt_handler!(216);
interrupt_handler!(217);
interrupt_handler!(218);
interrupt_handler!(219);
interrupt_handler!(220);
interrupt_handler!(221);
interrupt_handler!(222);
interrupt_handler!(223);
interrupt_handler!(224);
interrupt_handler!(225);
interrupt_handler!(226);
interrupt_handler!(227);
interrupt_handler!(228);
interrupt_handler!(229);
interrupt_handler!(230);
interrupt_handler!(231);
interrupt_handler!(232);
interrupt_handler!(233);
interrupt_handler!(234);
interrupt_handler!(235);
interrupt_handler!(236);
interrupt_handler!(237);
interrupt_handler!(238);
interrupt_handler!(239);
interrupt_handler!(240);
interrupt_handler!(241);
interrupt_handler!(242);
interrupt_handler!(243);
interrupt_handler!(244);
interrupt_handler!(245);
interrupt_handler!(246);
interrupt_handler!(247);
interrupt_handler!(248);
interrupt_handler!(249);
interrupt_handler!(250);
interrupt_handler!(251);
interrupt_handler!(252);
interrupt_handler!(253);
interrupt_handler!(254);
interrupt_handler!(255);

#[inline(never)]
pub unsafe fn arch_setup_interrupt_gate() {
    set_intr_gate(32, 0, VirtAddr::new(irq_handler32 as usize));
    set_intr_gate(33, 0, VirtAddr::new(irq_handler33 as usize));
    set_intr_gate(34, 0, VirtAddr::new(irq_handler34 as usize));
    set_intr_gate(35, 0, VirtAddr::new(irq_handler35 as usize));
    set_intr_gate(36, 0, VirtAddr::new(irq_handler36 as usize));
    set_intr_gate(37, 0, VirtAddr::new(irq_handler37 as usize));
    set_intr_gate(38, 0, VirtAddr::new(irq_handler38 as usize));
    set_intr_gate(39, 0, VirtAddr::new(irq_handler39 as usize));
    set_intr_gate(40, 0, VirtAddr::new(irq_handler40 as usize));

    set_intr_gate(41, 0, VirtAddr::new(irq_handler41 as usize));
    set_intr_gate(42, 0, VirtAddr::new(irq_handler42 as usize));
    set_intr_gate(43, 0, VirtAddr::new(irq_handler43 as usize));
    set_intr_gate(44, 0, VirtAddr::new(irq_handler44 as usize));
    set_intr_gate(45, 0, VirtAddr::new(irq_handler45 as usize));
    set_intr_gate(46, 0, VirtAddr::new(irq_handler46 as usize));
    set_intr_gate(47, 0, VirtAddr::new(irq_handler47 as usize));
    set_intr_gate(48, 0, VirtAddr::new(irq_handler48 as usize));
    set_intr_gate(49, 0, VirtAddr::new(irq_handler49 as usize));
    set_intr_gate(50, 0, VirtAddr::new(irq_handler50 as usize));

    set_intr_gate(51, 0, VirtAddr::new(irq_handler51 as usize));
    set_intr_gate(52, 0, VirtAddr::new(irq_handler52 as usize));
    set_intr_gate(53, 0, VirtAddr::new(irq_handler53 as usize));
    set_intr_gate(54, 0, VirtAddr::new(irq_handler54 as usize));
    set_intr_gate(55, 0, VirtAddr::new(irq_handler55 as usize));
    set_intr_gate(56, 0, VirtAddr::new(irq_handler56 as usize));
    set_intr_gate(57, 0, VirtAddr::new(irq_handler57 as usize));
    set_intr_gate(58, 0, VirtAddr::new(irq_handler58 as usize));
    set_intr_gate(59, 0, VirtAddr::new(irq_handler59 as usize));
    set_intr_gate(60, 0, VirtAddr::new(irq_handler60 as usize));

    set_intr_gate(61, 0, VirtAddr::new(irq_handler61 as usize));
    set_intr_gate(62, 0, VirtAddr::new(irq_handler62 as usize));
    set_intr_gate(63, 0, VirtAddr::new(irq_handler63 as usize));
    set_intr_gate(64, 0, VirtAddr::new(irq_handler64 as usize));
    set_intr_gate(65, 0, VirtAddr::new(irq_handler65 as usize));
    set_intr_gate(66, 0, VirtAddr::new(irq_handler66 as usize));
    set_intr_gate(67, 0, VirtAddr::new(irq_handler67 as usize));
    set_intr_gate(68, 0, VirtAddr::new(irq_handler68 as usize));
    set_intr_gate(69, 0, VirtAddr::new(irq_handler69 as usize));
    set_intr_gate(70, 0, VirtAddr::new(irq_handler70 as usize));

    set_intr_gate(71, 0, VirtAddr::new(irq_handler71 as usize));
    set_intr_gate(72, 0, VirtAddr::new(irq_handler72 as usize));
    set_intr_gate(73, 0, VirtAddr::new(irq_handler73 as usize));
    set_intr_gate(74, 0, VirtAddr::new(irq_handler74 as usize));
    set_intr_gate(75, 0, VirtAddr::new(irq_handler75 as usize));
    set_intr_gate(76, 0, VirtAddr::new(irq_handler76 as usize));
    set_intr_gate(77, 0, VirtAddr::new(irq_handler77 as usize));
    set_intr_gate(78, 0, VirtAddr::new(irq_handler78 as usize));
    set_intr_gate(79, 0, VirtAddr::new(irq_handler79 as usize));
    set_intr_gate(80, 0, VirtAddr::new(irq_handler80 as usize));

    set_intr_gate(81, 0, VirtAddr::new(irq_handler81 as usize));
    set_intr_gate(82, 0, VirtAddr::new(irq_handler82 as usize));
    set_intr_gate(83, 0, VirtAddr::new(irq_handler83 as usize));
    set_intr_gate(84, 0, VirtAddr::new(irq_handler84 as usize));
    set_intr_gate(85, 0, VirtAddr::new(irq_handler85 as usize));
    set_intr_gate(86, 0, VirtAddr::new(irq_handler86 as usize));
    set_intr_gate(87, 0, VirtAddr::new(irq_handler87 as usize));
    set_intr_gate(88, 0, VirtAddr::new(irq_handler88 as usize));
    set_intr_gate(89, 0, VirtAddr::new(irq_handler89 as usize));
    set_intr_gate(90, 0, VirtAddr::new(irq_handler90 as usize));

    set_intr_gate(91, 0, VirtAddr::new(irq_handler91 as usize));
    set_intr_gate(92, 0, VirtAddr::new(irq_handler92 as usize));
    set_intr_gate(93, 0, VirtAddr::new(irq_handler93 as usize));
    set_intr_gate(94, 0, VirtAddr::new(irq_handler94 as usize));
    set_intr_gate(95, 0, VirtAddr::new(irq_handler95 as usize));
    set_intr_gate(96, 0, VirtAddr::new(irq_handler96 as usize));
    set_intr_gate(97, 0, VirtAddr::new(irq_handler97 as usize));
    set_intr_gate(98, 0, VirtAddr::new(irq_handler98 as usize));
    set_intr_gate(99, 0, VirtAddr::new(irq_handler99 as usize));
    set_intr_gate(100, 0, VirtAddr::new(irq_handler100 as usize));

    set_intr_gate(101, 0, VirtAddr::new(irq_handler101 as usize));
    set_intr_gate(102, 0, VirtAddr::new(irq_handler102 as usize));
    set_intr_gate(103, 0, VirtAddr::new(irq_handler103 as usize));
    set_intr_gate(104, 0, VirtAddr::new(irq_handler104 as usize));
    set_intr_gate(105, 0, VirtAddr::new(irq_handler105 as usize));
    set_intr_gate(106, 0, VirtAddr::new(irq_handler106 as usize));
    set_intr_gate(107, 0, VirtAddr::new(irq_handler107 as usize));
    set_intr_gate(108, 0, VirtAddr::new(irq_handler108 as usize));
    set_intr_gate(109, 0, VirtAddr::new(irq_handler109 as usize));
    set_intr_gate(110, 0, VirtAddr::new(irq_handler110 as usize));

    set_intr_gate(111, 0, VirtAddr::new(irq_handler111 as usize));
    set_intr_gate(112, 0, VirtAddr::new(irq_handler112 as usize));
    set_intr_gate(113, 0, VirtAddr::new(irq_handler113 as usize));
    set_intr_gate(114, 0, VirtAddr::new(irq_handler114 as usize));
    set_intr_gate(115, 0, VirtAddr::new(irq_handler115 as usize));
    set_intr_gate(116, 0, VirtAddr::new(irq_handler116 as usize));
    set_intr_gate(117, 0, VirtAddr::new(irq_handler117 as usize));
    set_intr_gate(118, 0, VirtAddr::new(irq_handler118 as usize));
    set_intr_gate(119, 0, VirtAddr::new(irq_handler119 as usize));
    set_intr_gate(120, 0, VirtAddr::new(irq_handler120 as usize));

    set_intr_gate(121, 0, VirtAddr::new(irq_handler121 as usize));
    set_intr_gate(122, 0, VirtAddr::new(irq_handler122 as usize));
    set_intr_gate(123, 0, VirtAddr::new(irq_handler123 as usize));
    set_intr_gate(124, 0, VirtAddr::new(irq_handler124 as usize));
    set_intr_gate(125, 0, VirtAddr::new(irq_handler125 as usize));
    set_intr_gate(126, 0, VirtAddr::new(irq_handler126 as usize));
    set_intr_gate(127, 0, VirtAddr::new(irq_handler127 as usize));
    set_intr_gate(129, 0, VirtAddr::new(irq_handler129 as usize));
    set_intr_gate(130, 0, VirtAddr::new(irq_handler130 as usize));

    set_intr_gate(131, 0, VirtAddr::new(irq_handler131 as usize));
    set_intr_gate(132, 0, VirtAddr::new(irq_handler132 as usize));
    set_intr_gate(133, 0, VirtAddr::new(irq_handler133 as usize));
    set_intr_gate(134, 0, VirtAddr::new(irq_handler134 as usize));
    set_intr_gate(135, 0, VirtAddr::new(irq_handler135 as usize));
    set_intr_gate(136, 0, VirtAddr::new(irq_handler136 as usize));
    set_intr_gate(137, 0, VirtAddr::new(irq_handler137 as usize));
    set_intr_gate(138, 0, VirtAddr::new(irq_handler138 as usize));
    set_intr_gate(139, 0, VirtAddr::new(irq_handler139 as usize));
    set_intr_gate(140, 0, VirtAddr::new(irq_handler140 as usize));

    set_intr_gate(141, 0, VirtAddr::new(irq_handler141 as usize));
    set_intr_gate(142, 0, VirtAddr::new(irq_handler142 as usize));
    set_intr_gate(143, 0, VirtAddr::new(irq_handler143 as usize));
    set_intr_gate(144, 0, VirtAddr::new(irq_handler144 as usize));
    set_intr_gate(145, 0, VirtAddr::new(irq_handler145 as usize));
    set_intr_gate(146, 0, VirtAddr::new(irq_handler146 as usize));
    set_intr_gate(147, 0, VirtAddr::new(irq_handler147 as usize));
    set_intr_gate(148, 0, VirtAddr::new(irq_handler148 as usize));
    set_intr_gate(149, 0, VirtAddr::new(irq_handler149 as usize));
    set_intr_gate(150, 0, VirtAddr::new(irq_handler150 as usize));

    set_intr_gate(151, 0, VirtAddr::new(irq_handler151 as usize));
    set_intr_gate(152, 0, VirtAddr::new(irq_handler152 as usize));
    set_intr_gate(153, 0, VirtAddr::new(irq_handler153 as usize));
    set_intr_gate(154, 0, VirtAddr::new(irq_handler154 as usize));
    set_intr_gate(155, 0, VirtAddr::new(irq_handler155 as usize));
    set_intr_gate(156, 0, VirtAddr::new(irq_handler156 as usize));
    set_intr_gate(157, 0, VirtAddr::new(irq_handler157 as usize));
    set_intr_gate(158, 0, VirtAddr::new(irq_handler158 as usize));
    set_intr_gate(159, 0, VirtAddr::new(irq_handler159 as usize));
    set_intr_gate(160, 0, VirtAddr::new(irq_handler160 as usize));

    set_intr_gate(161, 0, VirtAddr::new(irq_handler161 as usize));
    set_intr_gate(162, 0, VirtAddr::new(irq_handler162 as usize));
    set_intr_gate(163, 0, VirtAddr::new(irq_handler163 as usize));
    set_intr_gate(164, 0, VirtAddr::new(irq_handler164 as usize));
    set_intr_gate(165, 0, VirtAddr::new(irq_handler165 as usize));
    set_intr_gate(166, 0, VirtAddr::new(irq_handler166 as usize));
    set_intr_gate(167, 0, VirtAddr::new(irq_handler167 as usize));
    set_intr_gate(168, 0, VirtAddr::new(irq_handler168 as usize));
    set_intr_gate(169, 0, VirtAddr::new(irq_handler169 as usize));
    set_intr_gate(170, 0, VirtAddr::new(irq_handler170 as usize));

    set_intr_gate(171, 0, VirtAddr::new(irq_handler171 as usize));
    set_intr_gate(172, 0, VirtAddr::new(irq_handler172 as usize));
    set_intr_gate(173, 0, VirtAddr::new(irq_handler173 as usize));
    set_intr_gate(174, 0, VirtAddr::new(irq_handler174 as usize));
    set_intr_gate(175, 0, VirtAddr::new(irq_handler175 as usize));
    set_intr_gate(176, 0, VirtAddr::new(irq_handler176 as usize));
    set_intr_gate(177, 0, VirtAddr::new(irq_handler177 as usize));
    set_intr_gate(178, 0, VirtAddr::new(irq_handler178 as usize));
    set_intr_gate(179, 0, VirtAddr::new(irq_handler179 as usize));
    set_intr_gate(180, 0, VirtAddr::new(irq_handler180 as usize));

    set_intr_gate(181, 0, VirtAddr::new(irq_handler181 as usize));
    set_intr_gate(182, 0, VirtAddr::new(irq_handler182 as usize));
    set_intr_gate(183, 0, VirtAddr::new(irq_handler183 as usize));
    set_intr_gate(184, 0, VirtAddr::new(irq_handler184 as usize));
    set_intr_gate(185, 0, VirtAddr::new(irq_handler185 as usize));
    set_intr_gate(186, 0, VirtAddr::new(irq_handler186 as usize));
    set_intr_gate(187, 0, VirtAddr::new(irq_handler187 as usize));
    set_intr_gate(188, 0, VirtAddr::new(irq_handler188 as usize));
    set_intr_gate(189, 0, VirtAddr::new(irq_handler189 as usize));
    set_intr_gate(190, 0, VirtAddr::new(irq_handler190 as usize));

    set_intr_gate(191, 0, VirtAddr::new(irq_handler191 as usize));
    set_intr_gate(192, 0, VirtAddr::new(irq_handler192 as usize));
    set_intr_gate(193, 0, VirtAddr::new(irq_handler193 as usize));
    set_intr_gate(194, 0, VirtAddr::new(irq_handler194 as usize));
    set_intr_gate(195, 0, VirtAddr::new(irq_handler195 as usize));
    set_intr_gate(196, 0, VirtAddr::new(irq_handler196 as usize));
    set_intr_gate(197, 0, VirtAddr::new(irq_handler197 as usize));
    set_intr_gate(198, 0, VirtAddr::new(irq_handler198 as usize));
    set_intr_gate(199, 0, VirtAddr::new(irq_handler199 as usize));

    set_intr_gate(200, 0, VirtAddr::new(irq_handler200 as usize));
    set_intr_gate(201, 0, VirtAddr::new(irq_handler201 as usize));
    set_intr_gate(202, 0, VirtAddr::new(irq_handler202 as usize));
    set_intr_gate(203, 0, VirtAddr::new(irq_handler203 as usize));
    set_intr_gate(204, 0, VirtAddr::new(irq_handler204 as usize));
    set_intr_gate(205, 0, VirtAddr::new(irq_handler205 as usize));
    set_intr_gate(206, 0, VirtAddr::new(irq_handler206 as usize));
    set_intr_gate(207, 0, VirtAddr::new(irq_handler207 as usize));
    set_intr_gate(208, 0, VirtAddr::new(irq_handler208 as usize));
    set_intr_gate(209, 0, VirtAddr::new(irq_handler209 as usize));
    set_intr_gate(210, 0, VirtAddr::new(irq_handler210 as usize));

    set_intr_gate(211, 0, VirtAddr::new(irq_handler211 as usize));
    set_intr_gate(212, 0, VirtAddr::new(irq_handler212 as usize));
    set_intr_gate(213, 0, VirtAddr::new(irq_handler213 as usize));
    set_intr_gate(214, 0, VirtAddr::new(irq_handler214 as usize));
    set_intr_gate(215, 0, VirtAddr::new(irq_handler215 as usize));
    set_intr_gate(216, 0, VirtAddr::new(irq_handler216 as usize));
    set_intr_gate(217, 0, VirtAddr::new(irq_handler217 as usize));
    set_intr_gate(218, 0, VirtAddr::new(irq_handler218 as usize));
    set_intr_gate(219, 0, VirtAddr::new(irq_handler219 as usize));
    set_intr_gate(220, 0, VirtAddr::new(irq_handler220 as usize));

    set_intr_gate(221, 0, VirtAddr::new(irq_handler221 as usize));
    set_intr_gate(222, 0, VirtAddr::new(irq_handler222 as usize));
    set_intr_gate(223, 0, VirtAddr::new(irq_handler223 as usize));
    set_intr_gate(224, 0, VirtAddr::new(irq_handler224 as usize));
    set_intr_gate(225, 0, VirtAddr::new(irq_handler225 as usize));
    set_intr_gate(226, 0, VirtAddr::new(irq_handler226 as usize));
    set_intr_gate(227, 0, VirtAddr::new(irq_handler227 as usize));
    set_intr_gate(228, 0, VirtAddr::new(irq_handler228 as usize));
    set_intr_gate(229, 0, VirtAddr::new(irq_handler229 as usize));
    set_intr_gate(230, 0, VirtAddr::new(irq_handler230 as usize));

    set_intr_gate(231, 0, VirtAddr::new(irq_handler231 as usize));
    set_intr_gate(232, 0, VirtAddr::new(irq_handler232 as usize));
    set_intr_gate(233, 0, VirtAddr::new(irq_handler233 as usize));
    set_intr_gate(234, 0, VirtAddr::new(irq_handler234 as usize));
    set_intr_gate(235, 0, VirtAddr::new(irq_handler235 as usize));
    set_intr_gate(236, 0, VirtAddr::new(irq_handler236 as usize));
    set_intr_gate(237, 0, VirtAddr::new(irq_handler237 as usize));
    set_intr_gate(238, 0, VirtAddr::new(irq_handler238 as usize));
    set_intr_gate(239, 0, VirtAddr::new(irq_handler239 as usize));
    set_intr_gate(240, 0, VirtAddr::new(irq_handler240 as usize));

    set_intr_gate(241, 0, VirtAddr::new(irq_handler241 as usize));
    set_intr_gate(242, 0, VirtAddr::new(irq_handler242 as usize));
    set_intr_gate(243, 0, VirtAddr::new(irq_handler243 as usize));
    set_intr_gate(244, 0, VirtAddr::new(irq_handler244 as usize));
    set_intr_gate(245, 0, VirtAddr::new(irq_handler245 as usize));
    set_intr_gate(246, 0, VirtAddr::new(irq_handler246 as usize));
    set_intr_gate(247, 0, VirtAddr::new(irq_handler247 as usize));
    set_intr_gate(248, 0, VirtAddr::new(irq_handler248 as usize));
    set_intr_gate(249, 0, VirtAddr::new(irq_handler249 as usize));
    set_intr_gate(250, 0, VirtAddr::new(irq_handler250 as usize));

    set_intr_gate(251, 0, VirtAddr::new(irq_handler251 as usize));
    set_intr_gate(252, 0, VirtAddr::new(irq_handler252 as usize));
    set_intr_gate(253, 0, VirtAddr::new(irq_handler253 as usize));
    set_intr_gate(254, 0, VirtAddr::new(irq_handler254 as usize));
    set_intr_gate(255, 0, VirtAddr::new(irq_handler255 as usize));
}

/// 设置中断门(DPL=0)
#[allow(dead_code)]
pub unsafe fn set_intr_gate(irq: u32, ist: u8, vaddr: VirtAddr) {
    let idt_entry = get_idt_entry(irq);
    set_gate(idt_entry, 0x8E, ist, vaddr);
}

/// 设置陷阱门(DPL=0)
#[allow(dead_code)]
pub unsafe fn set_trap_gate(irq: u32, ist: u8, vaddr: VirtAddr) {
    let idt_entry = get_idt_entry(irq);
    set_gate(idt_entry, 0x8F, ist, vaddr);
}

/// 设置系统调用门(DPL=3)
#[allow(dead_code)]
pub unsafe fn set_system_trap_gate(irq: u32, ist: u8, vaddr: VirtAddr) {
    let idt_entry = get_idt_entry(irq);
    set_gate(idt_entry, 0xEF, ist, vaddr);
}

unsafe fn get_idt_entry(irq: u32) -> &'static mut [u64] {
    assert!(irq < 256);
    let mut idt_vaddr =
        MMArch::phys_2_virt(PhysAddr::new(&IDT_Table as *const usize as usize)).unwrap();

    idt_vaddr += irq as usize * 16;

    let idt_entry = core::slice::from_raw_parts_mut(idt_vaddr.data() as *mut u64, 2);

    idt_entry
}
unsafe fn set_gate(gate: &mut [u64], attr: u8, ist: u8, handler: VirtAddr) {
    assert_eq!(gate.len(), 2);
    let mut d0: u64 = 0;
    let mut d1: u64 = 0;

    // 设置P、DPL、GateType
    d0 |= (attr as u64) << 40;
    // 设置IST
    d0 |= ((ist & 0x7) as u64) << 32;
    // 设置段选择子为0x10 ????
    d0 |= 0x8 << 16;

    let mut handler = handler.data() as u64;
    // 设置偏移地址[0:15]

    d0 |= handler & 0xFFFF;
    // 设置偏移地址[16:31]
    handler >>= 16;
    d0 |= (0xffff & handler) << 48;

    // 设置偏移地址[32:63]
    handler >>= 16;
    d1 |= handler & 0xFFFFFFFF;

    gate[0] = d0;
    gate[1] = d1;
}
