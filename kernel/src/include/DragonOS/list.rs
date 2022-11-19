use std::{cell::RefCell, rc::Rc, fmt::Debug, borrow::Borrow};

type Link<T> = Option<Rc<RefCell<Node<T>>>>;

// 链表的每个结点
#[derive(Debug)]
struct Node<T> {
    value: T,
    next: Link<T>,
    // prev: Link<T>,
}

// 链表表头
#[derive(Debug)]
struct List<T> {
    len: usize,
    first: Link<T>,
    rear: Link<T>,
}

impl<T> Node<T> {
    fn new(value: T) -> Rc<RefCell<Node<T>>> {
        let node = Rc::new(RefCell::new(Node {
            value,
            next: None,
            // prev: None,
        }));
        node
    }
}

impl<T: Copy + Debug + PartialEq> List<T> {
    fn new(value: T) -> List<T> {
        let first = Node::new(value);
        let rear = Rc::clone(&first);

        // first.as_ref().borrow_mut().next = Some(Rc::clone(&first));
        // rear.as_ref().borrow_mut().next = Some(Rc::clone(&rear));
        first.as_ref().borrow_mut().next = None;
        // rear.as_ref().borrow_mut().next = None;
        // println!("first 强引用计数：{:?}", Rc::strong_count(&first));
        List {
            len: 1,
            first: Some(first),
            rear: Some(rear),
        }
    }
    
    fn push(&mut self, value: T) {
        let new_node = Node::new(value);
        if let Some(f) = &self.rear {
            // println!("ref f = {:?}", f);
            f.as_ref().borrow_mut().next = Some(Rc::clone(&new_node));
        };
        self.rear = Some(Rc::clone(&new_node));
        // println!("new node 强引用计数：{:?}", Rc::strong_count(&new_node));
        self.len += 1;
    }

    // fn pop(&mut self) -> bool {
    //     let mut tmp: Link<T> = None;
    //     if let Some(f) = self.first.as_ref() {
    //         tmp = Some(Rc::clone(f));
    //         // let list = f.as_ref().borrow();
    //         // match &list.next {
    //         //     Some(next) => {
    //         //         f.as_ref().borrow_mut().next = Some(Rc::clone(next));

    //         //     },
    //         //     None => {
    //         //         // self.first = None;
    //         //     }
    //         // }
    //         // return true
    //     }
    //     let mut tmp_next: Link<T> = None;
    //     match &tmp.unwrap().as_ref().borrow_mut().next {
    //         Some(next) => {
    //             tmp_next = Some(Rc::clone(next));
    //         },
    //         None => {}
    //     }
    //     tmp.unwrap().as_ref().borrow_mut().next = None;
    //     return true;
    //     // false
    // }
}


#[test]
pub fn test() {
    let mut list = List::new(0);
    list.push(1);
    list.push(2);
    list.push(3);
    println!("{:#?}", list);
    // list.pop();
    // println!("{:#?}", list);
}
