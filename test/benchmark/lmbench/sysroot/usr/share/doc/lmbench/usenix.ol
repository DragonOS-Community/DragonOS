Introduction
	What is it?
		A bunch of speed of light benchmarks,
		not MP, not throughput, not saturation, not stress tests.
		A microbenchmark suite
		Measures system performance
			Latency and bandwidth measurements
		Measurements focus on OS and hardware
			What is delivered to the application
			Not marketing numbers
		Benchmark performance predicts application performance
	Results for which systems?
		Sun, SGI, DEC, IBM, HP, PCs
	Useful information to whom?
		Performance engineers, system programmers, system architects.
Motivation
	What are we measuring?
		Control / latecy operatins
		Bandwidth operations
	What aren't we measuring?
		Basic MIPS & MFLOPS.  XXX - not unless I do it right.
	What can I learn?
		Cost of operations
		****Operations per time unit****
		Compare speed of alternative paths (e.g. mmap vs. read)
	Performance problems = f(bw issues + latency issues)
	Give at least two examples
		NFS control & data: UDP lat, proc lat, & various BW metrics
		Oracle lock manager: TCP lat
		Verilog: mem lat
		AIM: fs ops XXX -ask Scott about pipes.
	Knowing the speeds of primitives can provide speeds of apps.
	An example here would be nice.
Outline
	Describe benchmark
		Give results from current machines
		Discuss results
	Future changes, enhancements, etc.
Tutorial on benchmarks
	For each metric
		what is it?
		why is it being measured?
		How is it measured?
		Measuring subtlities
		Interpreting the results
Latency 
	Process stuff
	networking stuff
	file system stuff
	memory stuff
	whatever
Bandwidth
	networking
	file system
	memory
Results
	Tabular results - XXX update that table to reflect the newer metrics
	Graphs of memory latency & context switches
	Discussion
		Memory stuff 
			Maybe contrast AIX with the $100K IBM
			uniprocessor w/ killer memory perf and point out
			that it is the memory that is making AIX go
			fast, it certainly isn't AIX.  A more politic
			observation would be that systems with good
			memory performace tend to have good system
			performance; the point being to shift people's
			attention to system performance, especially
			memory subsystem, as opposed to processor mips.
		Comparisons
			Maybe look at the table and draw attention to
			really good and really bad numbers for various
			platforms (like Linux' context switch time,
			Linux fs ops, solaris syscall, process stuff,
			990 memory BW).
Graphs
	A graph showing a range of really fast to really slow ops, all on the
	same graph.  Do bandwidth stuff normalized on MB/sec.
	Carl sez: show both ops/sec and cost/op on two graphs.
	A graph showing processor slow down due to memory misses, assuming 
	each instruction misses.  Maybe a graph that shows # of clocks
	(or better yet, # of instructions - think super scalar) that you would
	have to have between each memory miss in order to run at the clock
	speed.
War stories
	Sun page coloring bug
	SGI page coloring bug
	SGI hippi bug - XXX ask Thomas
	Sun bcopy bug
Lmbench [optional?]
	how to get lmbench
	how to compile
	how to run
	how to show results
Future work
	More hardware stuff - better latency measurements (write lat, 
	cache to cache latency). 
	add throughput & saturation measurements
TODO
	get some similar papers for comparison
	Someday I need reasonable I/O benchmarks to show off good
	big SMP machines like Challenge.
