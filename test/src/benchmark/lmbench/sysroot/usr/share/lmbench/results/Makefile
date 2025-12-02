# Makefile for lmbench results.
# $Id$
#
# Usage: make [ LIST="aix/* sunos/* ..." ] [ what ]
#
# What to make:
#	print			Prints the results 1 per page.
#	ps			Saves the postscript of 1 per page in PS/PS
#	4.ps			Saves the postscript of 4 per page in PS/PS4
#	8.ps			Saves the postscript of 8 per page in PS/PS8
#	x			Previews 1 per page using groff -X
#	summary	[default]	Ascii summary of the results
#	stats			Do statistics over a set of results
#	roff			Print the ascii summaries into a roff file
#	slides			Makes the pic for inclusion in slides
#
# This Makefile requires groff, gpic, and perl.  You could try it with
# other *roff processors; I have no idea if it works.
#
# XXX - this is all out of date.
#
# There are three sorts of graphical results:
#
# 1. Bargraphs comparing each system in the LIST on the measurements listed
#    in the BG list below (pretty much everything).
# 2. A 2-D graph for each system in LIST, displaying context switch times
#    as a function of (# of processes, size of each process).
# 3. A 2-D graph for each system in LIST, displaying memory read times as
#    a function of (stride size, memory size).
#
# The bargraphs are in a format of my own - the perl script in scripts
# called bargraph takes them as input and produces pic as output.
# It is a pretty straightforward format, you could probably incorparate
# into some Windows spreadsheet if you wanted to.  See tmp/*.bg after
# running make in this directory.
#
# The 2-D graphs are in a format that can (probably) be read by Xgraph.
# I've added a few extensions for titles, etc., that you could just
# take out.  See tmp/mem.* after running a make in this directory.
#
# This Makefile is of marginal usefulness to a site with just one machine.
# I intend to make results available so that people can compare, as well
# as a service where you can compare your results against the "best of
# the breed" for each vendor, as well as against best of the lot.

# List of result files to process.  Defaults to everything.
LIST=	`$(SCRIPTS)getlist $(LST)`	

# Grrrrr
SHELL=/bin/sh

SCRIPTS=../scripts/
SRCS= ../scripts/allctx ../scripts/allmem ../scripts/bargraph \
	../scripts/bghtml ../scripts/getbg ../scripts/getbw \
	../scripts/getctx ../scripts/getdisk ../scripts/getlist \
	../scripts/getmax ../scripts/getmem ../scripts/getpercent \
	../scripts/getresults ../scripts/getsummary ../scripts/gifs \
	../scripts/graph ../scripts/html-list ../scripts/html-man \
	../scripts/os ../scripts/percent ../scripts/save \
	../scripts/stats ../scripts/xroff 

MISC=	tmp/misc_mhz.bg \
	tmp/lat_ctx.bg \
	tmp/lat_ctx8.bg \
	tmp/lat_nullsys.bg \
	tmp/lat_signal.bg \
	tmp/lat_pagefault.bg \
	tmp/lat_mappings.bg \
	tmp/lat_fs_create.bg

PROC=	tmp/lat_nullproc.bg \
	tmp/lat_simpleproc.bg \
	tmp/lat_shproc.bg

LATENCY= \
	tmp/lat_pipe.bg \
	tmp/lat_connect.bg \
	tmp/lat_udp_local.bg \
	tmp/lat_rpc_udp_local.bg \
	tmp/lat_tcp_local.bg  \
	tmp/lat_rpc_tcp_local.bg 

BANDWIDTH= \
	tmp/bw_pipe.bg \
	tmp/bw_tcp_local.bg \
	tmp/bw_file.bg \
	tmp/bw_reread.bg \
	tmp/bw_mmap.bg \
	tmp/bw_bcopy_libc.bg \
	tmp/bw_bcopy_unrolled.bg \
	tmp/bw_mem_rdsum.bg \
	tmp/bw_mem_wr.bg

BG=	$(MISC) $(PROC) $(LATENCY) $(BANDWIDTH)

MK=@$(MAKE) -s
PRINT=groff -p | lpr -h
PS=groff -p | $(SCRIPTS)save PS/PS
PS8UP=groff -p | mpage -P- -8 -a | $(SCRIPTS)save PS/PS8
PS4UP=groff -p | mpage -P- -4 -a | $(SCRIPTS)save PS/PS4
SIZE=-big 
IMAGE=pbm
CLOSE=
GMEM=$(CLOSE) -grid -logx -xm -below
GCTX=$(CLOSE) -grid -below
GDISK=-below -close -grid -nolines
#IMAGE=gifmono

summary: $(SRCS)
	@$(SCRIPTS)getsummary $(LIST)

percent: $(SRCS)
	@$(SCRIPTS)getpercent $(LIST)

stats: $(SRCS)
	$(SCRIPTS)getsummary $(LIST) | $(SCRIPTS)percent

roff:
	echo .nf	> summary.roff
	echo .ft CB	>> summary.roff
	echo .ps 12	>> summary.roff
	echo .po .35i	>> summary.roff
	echo .sp .5i	>> summary.roff
	make LIST="$(LIST)" summary	>> summary.roff
	echo .bp	>> summary.roff
	echo .sp .5i	>> summary.roff
	make LIST="$(LIST)" percent	>> summary.roff

list:
	@echo $(LIST)

print: ctx mem disk bwfile bwmem

8:
	$(MK) LIST="$(LIST)" PRINT="groff -p | mpage -P -8 -a | lpr -h" print

8.ps 8ps 8up:
	$(MK) LIST="$(LIST)" PRINT="$(PS8UP)" print

4.ps 4ps 4up:
	$(MK) LIST="$(LIST)" PRINT="$(PS4UP)" print

ps:
	$(MK) LIST="$(LIST)" PRINT="$(PS)" print

smallps:
	$(MK) LIST="$(LIST)" SIZE= PRINT="groff -p | $(SCRIPTS)save PS/smallPS" print

x: 
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" print

ctx.x: 
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" ctx

ctx.ps4:
	$(MK) LIST="$(LIST)" PRINT="$(PS4UP)" ctx

mem.x: 
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" mem

disk.x: 
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" disk

bwfile.ps: 
	$(MK) LIST="$(LIST)" PRINT="$(PS)" bwfile

bwfile.x: 
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" bwfile

bwmem.ps: 
	$(MK) LIST="$(LIST)" PRINT="$(PS)" bwmem

bwmem.x: 
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" bwmem

smallx:
	$(MK) LIST="$(LIST)" PRINT="$(SCRIPTS)xroff -p" SIZE= print

slides:
	$(MK) LIST="$(LIST)" SIZE=-slide bargraphs.slides ctx.slides mem.slides

paper:
	$(MK) LIST="$(LIST)" tbl.paper ctx.paper mem.paper

# XXX - this has to be made incremental, doing everything over from
# scratch makes you want a Ghz machine.
html: dirs
	-make clean
	#$(SCRIPTS)bghtml $(BG)
	$(SCRIPTS)html-list $(LIST)
	$(MK) LIST="$(LIST)" summary > HTML/summary.out 2> HTML/summary.errs
	#make LIST="$(LIST)" percent > HTML/percent.out 2> HTML/percent.errs
	$(MK) LIST="$(LIST)" SIZE=  PRINT="$(PS)" \
	    GMEM="$(GMEM) -cut -gthk1" GCTX="$(GCTX) -cut -gthk1" print
	$(MK) LIST="$(LIST)" SIZE= NOOP=-noop PRINT="$(PS)" \
	    GMEM="$(GMEM) -cut -gthk1" GCTX="$(GCTX) -cut -gthk1" print
	gs -sOutputFile=HTML/ctx%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS < /dev/null
	gs -sOutputFile=HTML/mem%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.1 < /dev/null
	gs -sOutputFile=HTML/disk%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.2 < /dev/null
	gs -sOutputFile=HTML/bwfile%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.3 < /dev/null
	gs -sOutputFile=HTML/bwmem%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.4 < /dev/null
	gs -sOutputFile=HTML/ctx-unscaled%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.5 < /dev/null
	gs -sOutputFile=HTML/mem-unscaled%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.6 < /dev/null
	gs -sOutputFile=HTML/bwfile-unscaled%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.7 < /dev/null
	gs -sOutputFile=HTML/bwmem-unscaled%02d.$(IMAGE) -sDEVICE=$(IMAGE) -q -dNOPAUSE PS/PS.8 < /dev/null
	$(SCRIPTS)/gifs
	rm HTML/*.pbm HTML/___tmp*

htmltest: dirs
	-make clean
	#$(SCRIPTS)bghtml $(BG)
	$(SCRIPTS)html-list $(LIST)
	$(MK) LIST="$(LIST)" summary > HTML/summary.out 2> HTML/summary.errs
	#make LIST="$(LIST)" percent > HTML/percent.out 2> HTML/percent.errs
	$(MK) LIST="$(LIST)" SIZE=  PRINT="$(PS)" \
	    GMEM="$(GMEM) -cut -gthk1" GCTX="$(GCTX) -cut -gthk1" print

bghtml:
	$(SCRIPTS)bghtml $(BG)

html-list:
	$(SCRIPTS)html-list $(LIST)

ctx: dirs
	$(SCRIPTS)getctx $(LIST) > tmp/FILES
	@if [ -s tmp/FILES ]; \
	then	$(SCRIPTS)getmax $(NOOP) -graph `cat tmp/FILES`; \
		for i in `cat tmp/FILES`; \
		do	$(SCRIPTS)graph $(SIZE) $(GCTX) $$i; \
			echo .bp; \
		done | sed '$$d' | $(PRINT); \
	else	echo No context switch data in $(LIST); \
	fi

disk: dirs
	if [ X$(NOOP) = X ]; then \
		$(SCRIPTS)getdisk $(LIST) > tmp/FILES; \
		if [ -s tmp/FILES ]; \
		then	for i in `cat tmp/FILES`; \
			do	$(SCRIPTS)graph $(SIZE) $(GDISK) $$i; \
				echo .bp; \
        		done | sed '$$d' | $(PRINT); \
		else	echo No disk data in $(LIST); \
		fi; \
	fi

mem: dirs
	$(SCRIPTS)getmem $(LIST) > tmp/FILES
	if [ -s tmp/FILES ]; \
	then	$(SCRIPTS)getmax $(NOOP) -graph `cat tmp/FILES`; \
		for i in `cat tmp/FILES`; \
		do	$(SCRIPTS)graph $(SIZE) $(GMEM) -nomarks $$i; \
			echo .bp; \
        	done | sed '$$d' | $(PRINT); \
	else	echo No memory latency data in $(LIST); \
	fi

bwfile: dirs
	$(SCRIPTS)getbw $(LIST) > tmp/FILES
	if [ -s tmp/FILES ]; \
	then	$(SCRIPTS)getmax $(NOOP) -graph `cat tmp/FILES`; \
		for i in `cat tmp/FILES`; \
		do	$(SCRIPTS)graph $(SIZE) $(GMEM) -logy $$i; \
			echo .bp; \
        	done | sed '$$d' | $(PRINT); \
	else	echo No file bandwidth data in $(LIST); \
	fi

bwmem: dirs
	$(SCRIPTS)getbw -all $(LIST) > tmp/FILES
	if [ -s tmp/FILES ]; \
	then	$(SCRIPTS)getmax $(NOOP) -graph `cat tmp/FILES`; \
		for i in `cat tmp/FILES`; \
		do	$(SCRIPTS)graph -halfgrid -gthk_5 -thk2 -medium \
			    -nomarks -nolabels -grapheach $(GMEM) \
			    -logy %P="'`basename $$i`'" $$i; \
			echo .bp; \
        	done | sed '$$d' | $(PRINT); \
	else	echo No memory bandwidth data in $(LIST); \
	fi

tbl.paper:
	$(SCRIPTS)getbg -paper $(LIST) 


bargraphs.1st: dirs
	$(SCRIPTS)getbg -nosort $(LIST)
	#$(SCRIPTS)getmax -v $(PROC)
	#$(SCRIPTS)getmax -v $(LATENCY)
	#$(SCRIPTS)getmax -v -half $(BANDWIDTH)

bargraphs: bargraphs.1st
	for i in $(BG); \
	do	$(SCRIPTS)bargraph $(SIZE) -nobox -sideways $$i; \
		echo .bp; \
        done | sed '$$d' | $(PRINT)

bargraphs.slides: bargraphs.1st
	for i in $(BG); \
	do	$(SCRIPTS)bargraph $(SIZE) -nobox -sideways $$i > $${i}.pic; \
        done 

bargraphs.8up: bargraphs.1st
	for i in $(BG); \
	do	$(SCRIPTS)bargraph -sideways $(SIZE) -nobox $$i; \
		echo .bp; \
	done | sed '$$d' | $(PS8UP)

latency.8up: bargraphs.1st
	for i in $(LATENCY); \
	do	$(SCRIPTS)bargraph -sideways $(SIZE) -nobox $$i; \
		echo .bp; \
	done | sed '$$d' | $(PS8UP)

bw.8up: bargraphs.1st
	for i in $(BANDWIDTH); \
	do	$(SCRIPTS)bargraph -sideways $(SIZE) -nobox $$i; \
		echo .bp; \
	done | sed '$$d' | $(PS8UP)

get:	# nothing to do

clean:
	/bin/rm -f PS/* GIF/* HTML/* tmp/* summary.roff
	-bk clean

distclean:
	/bin/rm -fr PS  GIF   HTML   tmp   summary.roff

dirs:
	@if [ ! -d tmp ]; then mkdir tmp; fi
	@if [ ! -d PS ]; then mkdir PS; fi
	@if [ ! -d HTML ]; then mkdir HTML; fi
