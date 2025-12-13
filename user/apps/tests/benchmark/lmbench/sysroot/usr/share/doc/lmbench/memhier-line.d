frame invis ht 1.5 wid 2.5 left solid bot solid
label top "\fBalpha Linux 2.2.16-3\fR" down 0.3
label bot "Line Size (Bytes)"
label left "Latency (ns)"
coord log log
ticks bottom out from 8 to 512 by *4
ticks bottom out from 8 to 512 by *2 ""
draw solid
8    7.247
16   10.909
32   16.788
64   17.083
128   16.272
256   16.721
512   16.129
"L1" rjust above at 512,  16.129
draw solid
8   22.853
16   41.496
32   78.712
64  141.658
128  139.119
256  138.446
512  137.902
"L2" rjust above at 512, 137.902
draw solid
8   51.529
16   98.915
32  193.614
64  372.230
128  371.689
256  371.486
512  371.486
"L3" rjust above at 512, 371.486
