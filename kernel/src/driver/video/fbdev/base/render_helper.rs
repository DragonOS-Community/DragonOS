// use alloc::collections::vec_deque::Iter;

use core::slice::Iter;

pub struct BitItor<'a>{
    fgcolor:u32,
    bkcolor:u32,
    color_pattern:EndianPattern,
    dst_pattern:EndianPattern,
    src:Iter<'a,u8>,
    read_mask:u8,
    byte_per_pixel:u32,
    buffer:u32,
    current:u8,
    left_byte:u32,
    done:bool,
}

impl<'a> BitItor<'a>{
    pub fn new(fgcolor:u32,bkcolor:u32,dst_pattern:EndianPattern,color_pattern:EndianPattern,byte_per_pixel:u32,src:Iter<'a,u8>)->Self{
        let mut fgcolor=fgcolor;
        let mut bkcolor=bkcolor;
        if dst_pattern!=color_pattern{
            fgcolor=Self::reverse(fgcolor,byte_per_pixel);
            bkcolor=Self::reverse(bkcolor,byte_per_pixel);
        }
        
        let mut ans=Self{
            fgcolor,
            bkcolor,
            color_pattern,
            dst_pattern,
            src:src,
            read_mask:0b10000000,
            byte_per_pixel,
            buffer:0,
            current:0,
            left_byte:0,
            done:false,
        };
        ans.current=ans.src.next().unwrap().clone();
        return ans;
    }

    fn reverse(num:u32,byte_per_pixel:u32)->u32{
        let mask=0x000000ff;
        let mut ans=0;
        let mut num=num;
        for _ in 0..3{
            ans|=mask&num;
            ans<<=8;
            num>>=8;
        }
        ans|=mask&num;
        ans>>=(4-byte_per_pixel)*8;
        return ans;
    }

    fn move_mask(&mut self)->bool{
        self.read_mask>>=1;
        if self.read_mask==0b000000000{
            self.read_mask=0b10000000;
            self.current=match self.src.next(){
                Some(x)=>{
                    // println!("x:{:?}",x);
                    x.clone()
                },
                None=>{
                    // println!("x:None",);
                    return false;
                }
            };
            return true;
        }else{
            return true;
        }
    }

    fn full_buffer(&mut self)->Result<u32,u32>{
        let mut color=self.read_bit();
        let mut buffer_pointer=0;
        let mask=0x000000ff;
        let mut temp=0;
        while buffer_pointer<4{
            temp=(color&(mask<<self.left_byte*8));
            // temp<<=(buffer_pointer-self.left_byte) as u32;
            
            if buffer_pointer>self.left_byte{
                temp<<=(buffer_pointer-self.left_byte)*8
            }else{
                temp>>=(self.left_byte-buffer_pointer)*8
            }
            
            self.buffer|=temp;
            // println!("temp1:{}",self.buffer);
            buffer_pointer+=1;
            self.left_byte+=1;
            // println!("{},{}",buffer_pointer,self.left_byte);
            if self.left_byte>=self.byte_per_pixel{
                self.left_byte=0;
                if !self.move_mask(){
                    return Err(self.buffer);
                }
                color=self.read_bit();
            }
        }
        return Ok(self.buffer);
    }

    // fn write_buffer(index:u32,dst:&mut u32,src:&u32){
        
    // }

    fn read_bit(&self)->u32{
        match self.read_mask&self.current{
            0=>{
                self.bkcolor
            },
            _=>{
                self.fgcolor
            }
        }
    }
}

impl Iterator for BitItor<'_>{
    type Item = u32;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done{
            return None;
        }
        match self.full_buffer(){
            Ok(x)=>{
                self.buffer=0;
                return Some(x);
            },
            Err(x)=>{
                self.done=true;
                return Some(x);
            }
        }
        
    }
}
#[derive(PartialEq, PartialOrd)]
pub enum EndianPattern{
    BigEndian,
    LittleEndian,
}