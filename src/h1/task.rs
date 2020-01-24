use super::http11::{try_parse_http11, write_http11_req};
use super::Error;
use std::ops::Deref;
use std::task::Waker;

const HEADER_BUF_SIZE: usize = 1024;
const RECV_BODY_SIZE: usize = 16_384;

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Seq(pub usize);
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct End(pub bool);
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TaskId(pub usize);

pub struct TaskInfo {
    pub seq: Seq,
    pub task_id: TaskId,
}

impl TaskInfo {
    pub fn new(seq: Seq) -> Self {
        TaskInfo {
            seq,
            task_id: TaskId(0),
        }
    }
}

#[allow(clippy::large_enum_variant)]
pub enum Task {
    SendReq(SendReq),
    SendBody(SendBody),
    RecvRes(RecvRes),
    RecvBody(RecvBody),
}

impl Task {
    pub fn info(&self) -> &TaskInfo {
        match self {
            Task::SendReq(t) => &t.info,
            Task::SendBody(t) => &t.info,
            Task::RecvRes(t) => &t.info,
            Task::RecvBody(t) => &t.info,
        }
    }

    pub fn info_mut(&mut self) -> &mut TaskInfo {
        match self {
            Task::SendReq(t) => &mut t.info,
            Task::SendBody(t) => &mut t.info,
            Task::RecvRes(t) => &mut t.info,
            Task::RecvBody(t) => &mut t.info,
        }
    }

    pub fn is_send_req(&self) -> bool {
        if let Task::SendReq(_) = self {
            return true;
        }
        false
    }

    pub fn is_send_body(&self) -> bool {
        if let Task::SendBody(_) = self {
            return true;
        }
        false
    }

    pub fn is_recv_res(&self) -> bool {
        if let Task::RecvRes(_) = self {
            return true;
        }
        false
    }

    pub fn is_recv_body(&self) -> bool {
        if let Task::RecvBody(_) = self {
            return true;
        }
        false
    }
}

pub struct SendReq {
    pub info: TaskInfo,
    pub req: Vec<u8>,
    pub end: End,
}

impl SendReq {
    pub fn from_req(seq: Seq, req: http::Request<()>, end: bool) -> Result<Self, Error> {
        let mut req_buf = vec![0; HEADER_BUF_SIZE];
        let size = write_http11_req(&req, &mut req_buf[..])?;
        req_buf.resize(size, 0);
        Ok(SendReq {
            info: TaskInfo::new(seq),
            req: req_buf,
            end: End(end),
        })
    }
}

pub struct SendBody {
    pub info: TaskInfo,
    pub body: Vec<u8>,
    pub end: End,
    pub send_waker: Option<Waker>,
}

impl SendBody {
    pub fn new(seq: Seq, body: Vec<u8>, end: bool) -> Self {
        SendBody {
            info: TaskInfo::new(seq),
            body,
            end: End(end),
            send_waker: None,
        }
    }
}

pub struct RecvRes {
    pub info: TaskInfo,
    pub buf: Vec<u8>,
    pub waker: Waker,
}

impl RecvRes {
    pub fn new(seq: Seq, waker: Waker) -> Self {
        RecvRes {
            info: TaskInfo::new(seq),
            buf: Vec::with_capacity(HEADER_BUF_SIZE),
            waker,
        }
    }
}

pub struct RecvBody {
    pub info: TaskInfo,
    pub buf: Vec<u8>,
    pub end: End,
    pub waker: Waker,
}

impl RecvBody {
    pub fn new(seq: Seq, waker: Waker) -> Self {
        RecvBody {
            info: TaskInfo::new(seq),
            buf: Vec::with_capacity(RECV_BODY_SIZE),
            end: End(false),
            waker,
        }
    }
}

pub struct Tasks {
    next_task_id: usize,
    list: Vec<Task>,
}

impl Tasks {
    pub fn new() -> Self {
        Tasks {
            next_task_id: 0,
            list: vec![],
        }
    }

    pub fn push(&mut self, mut task: Task) {
        let task_id = self.next_task_id;
        self.next_task_id += 1;
        task.info_mut().task_id = TaskId(task_id);
        self.list.push(task);
    }

    pub fn remove(&mut self, task_id: TaskId) {
        self.list.retain(|t| t.info().task_id != task_id);
    }

    fn get_task<F: Fn(&Task) -> bool>(&mut self, seq: Seq, func: F) -> Option<&mut Task> {
        self.list
            .iter_mut()
            .find(|t| t.info().seq == seq && (func)(t))
    }

    pub fn get_send_req(&mut self, seq: Seq) -> Option<&mut SendReq> {
        match self.get_task(seq, Task::is_send_req) {
            Some(Task::SendReq(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_send_body(&mut self, seq: Seq) -> Option<&mut SendBody> {
        match self.get_task(seq, Task::is_send_body) {
            Some(Task::SendBody(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_recv_res(&mut self, seq: Seq) -> Option<&mut RecvRes> {
        match self.get_task(seq, Task::is_recv_res) {
            Some(Task::RecvRes(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_recv_body(&mut self, seq: Seq) -> Option<&mut RecvBody> {
        match self.get_task(seq, Task::is_recv_body) {
            Some(Task::RecvBody(t)) => Some(t),
            _ => None,
        }
    }
}

impl Deref for Seq {
    type Target = usize;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Deref for End {
    type Target = bool;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Deref for TaskId {
    type Target = usize;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<SendReq> for Task {
    fn from(v: SendReq) -> Self {
        Task::SendReq(v)
    }
}

impl From<SendBody> for Task {
    fn from(v: SendBody) -> Self {
        Task::SendBody(v)
    }
}

impl From<RecvRes> for Task {
    fn from(v: RecvRes) -> Self {
        Task::RecvRes(v)
    }
}

impl From<RecvBody> for Task {
    fn from(v: RecvBody) -> Self {
        Task::RecvBody(v)
    }
}
