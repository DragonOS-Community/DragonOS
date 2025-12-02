# Makefile for lmbench doc subdir.
# $Id$

SHELL=/bin/sh
DESC = description.ms 
USENIX = tmac.usenix usenix96.ms 
PIC = ctx.pic mem.pic 
SCRIPTS = ../scripts/
BASE=/usr/local
MANDIR=${BASE}/man

MAN = \
	bargraph.1 graph.1 						\
	lmbench.3 reporting.3 results.3 timing.3 			\
	lmbench.8 mhz.8 cache.8 line.8 tlb.8 lmdd.8			\
	lat_proc.8 lat_mmap.8 lat_ctx.8 lat_syscall.8 lat_pipe.8 	\
	lat_http.8 lat_tcp.8 lat_udp.8 lat_rpc.8 lat_connect.8 lat_fs.8	\
	lat_ops.8 lat_pagefault.8 lat_mem_rd.8 lat_select.8		\
	lat_fifo.8 lat_fcntl.8 lat_sig.8 lat_unix.8 lat_unix_connect.8	\
	bw_file_rd.8 bw_mem.8 bw_mmap_rd.8				\
	bw_pipe.8 bw_tcp.8 bw_unix.8 					\
	par_ops.8 par_mem.8

ALL = $(DESC) $(USENIX) $(PIC) $(MAN) $(REFER) references

.SUFFIXES: .pic .fig

.fig.pic:
	fig2dev -L pic $< $*.pic

PS ps: $(ALL)
	gindxbib references
	groff -t -e -G -s -p -R $(USENIX) > USENIX.PS
	#groff -s -p -mgs $(DESC) > DESC.PS
	#groff -fH -man $(MAN) > MAN.PS

X x: $(ALL)
	gindxbib references
	$(SCRIPTS)xroff -t -e -s -p -R $(USENIX) 
	#$(SCRIPTS)xroff -s -p -mgs $(DESC) 
	#$(SCRIPTS)xroff -man -fH $(MAN) 

text: $(ALL)
	gindxbib references
	gsoelim usenix96.ms | sed "s/expand doublebox/center/" | \
	sed s/doublebox// > Fixed.ms
	groff -Tascii -t -e -s -p -R -mgs Fixed.ms 2>/dev/null | colcrt - | more

userguide.ps: $(ALL) references-userguide userguide.ms \
		lmbench3_arch.pic lmbench3_signals.pic ctx.tbl \
		bw_allmem.tbl bw_ipc.tbl bw_reread2.tbl bw_tcp.tbl \
		lat_allmem.tbl lat_allproc.tbl lat_connect.tbl \
		lat_disk.tbl lat_fs.tbl lat_ipc.tbl lat_nullsys.tbl \
		lat_pipe.tbl lat_signal.tbl lat_tcp.tbl lat_udp.tbl
	gindxbib references-userguide
	groff -t -e -G -s -p -R tmac.usenix userguide.ms > userguide.ps

memhier.ps: $(ALL) memhier-color.d memhier-tlb.d memhier-line.d references-memhier memhier.ms
	gindxbib references-memhier
	groff -G -t -e -s -p -R tmac.usenix memhier.ms > memhier.ps
#	../scripts/graph -xm -logx -small -below -nomarks -nospace memhier-color.graph > memhier-color.pic
#	../scripts/graph -xm -logx -small -below -nomarks -nospace memhier-line.graph > memhier-line.pic
#	../scripts/graph -logx -small -below -nomarks -nospace memhier-tlb.graph > memhier-tlb.pic

lmbench3.ps: $(ALL) references-lmbench3 lmbench3.ms \
		lmbench3_arch.pic lmbench3_signals.pic
	gindxbib references-lmbench3
	groff -G -t -e -s -p -R tmac.usenix lmbench3.ms > lmbench3.ps

parallel.ps: $(ALL) references-parallel parallel.ms
	gindxbib references-parallel
	groff -G -t -e -s -p -R tmac.usenix parallel.ms > parallel.ps

install: $(MAN)
	for f in $(MAN); do \
		for s in 1 2 3 4 5 6 7 8 9; do \
			if [ ! -d ${MANDIR}/man$${s} ]; then \
				mkdir -p ${MANDIR}/man$${s}; \
			fi; \
			base=`basename $${f} .$${s}`; \
			if [ "$${base}.$${s}" = "$$f" ]; then \
				cp $$f ${MANDIR}/man$${s}/; \
			fi; \
		done; \
	done

get: $(ALL)

edit:
	get -e -s $(ALL)

$(MAN):
	get -s $(MAN)

$(PIC):
	get -s $(PIC)

$(DESC):
	get -s $(DESC)

$(USENIX):
	get -s $(USENIX)

clean:
	-bk clean 
	/bin/rm -f *.PS XXX bw.pic memrd_bcopy_comp.pic references.i
