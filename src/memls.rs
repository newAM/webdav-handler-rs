//! Simple in-memory locksystem.
//!
//! This implementation has state - if you create a
//! new instance in a handler(), it will be empty every time.
//!
//! So you have to create the instance once, using `MemLs::new`, store
//! it in your handler struct, and clone() it every time you pass
//! it to the DavHandler. Cloning is ofcourse not expensive, the
//! MemLs handle is refcounted, obviously.
use std::time::{SystemTime,Duration};
use std::sync::{Arc,Mutex};
use std::collections::HashMap;

use uuid::Uuid;
use xmltree::Element;

use webpath::WebPath;
use tree;
use ls::*;

type Tree = tree::Tree<Vec<u8>, Vec<DavLock>>;

#[derive(Debug, Clone)]
pub struct MemLs(Arc<Mutex<MemLsInner>>);

#[derive(Debug)]
struct MemLsInner {
    tree:   Tree,
    locks:  HashMap<Vec<u8>, u64>,
}

impl MemLs {
    /// Create a new "memls" locksystem.
    pub fn new() -> Box<MemLs> {
        let inner = MemLsInner{
            tree:   Tree::new(Vec::new()),
            locks:  HashMap::new(),
        };
        Box::new(MemLs(Arc::new(Mutex::new(inner))))
    }
}

impl DavLockSystem for MemLs {

    fn lock(&self, path: &WebPath, owner: Option<Element>, timeout: Option<Duration>, shared: bool, deep: bool) -> Result<DavLock, DavLock> {
        let inner = &mut *self.0.lock().unwrap();

        // any locks in the path?
        check_locks_to_path(&inner.tree, path, Vec::new(), shared)?;

        // if it's a deep lock we need to check if there are locks furter along the path.
        if deep {
            check_locks_from_path(&inner.tree, path, shared)?;
        }

        // create lock.
        let node = get_or_create_path_node(&mut inner.tree, path);
        let timeout_at = match timeout {
            None => None,
            Some(d) => Some(SystemTime::now() + d),
        };
        let lock = DavLock{
            token:      Uuid::new_v4().urn().to_string(),
            path:       path.clone(),
            owner:      owner,
            timeout_at: timeout_at,
            timeout:    timeout,
            shared:     shared,
            deep:       deep,
        };
        let slock = lock.clone();
        node.push(slock);
        Ok(lock)
    }

    fn unlock(&self, path: &WebPath, token: &str) -> Result<(), ()> {
        let inner = &mut *self.0.lock().unwrap();
        let node_id = match lookup_lock(&inner.tree, path, token) {
            None => return Err(()),
            Some(n) => n,
        };
        let len = {
            let node = inner.tree.get_node_mut(node_id).unwrap();
            let idx = node.iter().position(|n| n.token.as_str() == token).unwrap();
            node.remove(idx);
            node.len()
        };
        if len == 0 {
            inner.tree.delete_node(node_id).ok();
        }
        Ok(())
    }

    fn refresh(&self, path: &WebPath, token: &str, timeout: Option<Duration>) -> Result<DavLock, ()> {
        let inner = &mut *self.0.lock().unwrap();
        let node_id = match lookup_lock(&inner.tree, path, token) {
            None => return Err(()),
            Some(n) => n,
        };
        let node = (&mut inner.tree).get_node_mut(node_id).unwrap();
        let idx = node.iter().position(|n| n.token.as_str() == token).unwrap();
        let lock = &mut node[idx];
        let timeout_at = match timeout {
            None => None,
            Some(d) => Some(SystemTime::now() + d),
        };
        lock.timeout = timeout;
        lock.timeout_at = timeout_at;
        Ok(lock.clone())
    }

    fn check(&self, path: &WebPath, submitted_tokens: Vec<&str>) -> Result<(), DavLock> {
        let inner = &*self.0.lock().unwrap();
        check_locks_to_path(&inner.tree, path, submitted_tokens, false)
    }

    fn discover(&self, path: &WebPath) -> Vec<DavLock> {
        let inner = &*self.0.lock().unwrap();
        list_locks(&inner.tree, path)
    }

    fn delete(&self, path: &WebPath) -> Result<(), ()> {
        let inner = &mut *self.0.lock().unwrap();
        if let Some(node_id) = lookup_node(&inner.tree, path) {
            (&mut inner.tree).delete_subtree(node_id).ok();
        }
        Ok(())
    }
}

// check if there are any locks along the path.
fn check_locks_to_path(tree: &Tree, path: &WebPath, submitted_tokens: Vec<&str>, shared_ok: bool) -> Result<(), DavLock> {

    // split path into segments, starting with an empty segment indicating root ("/").
    let path = path.as_bytes();
    let mut segs : Vec<&[u8]> = path.split(|&c| c == b'/').filter(|s| s.len() > 0).collect();
    segs.insert(0, b"");
    let last_seg = segs.len() - 1;

    // state
    let mut holds_lock = false;
    let mut first_lock_seen : Option<&DavLock> = None;

    // walk over path segments starting at root.
    let mut node_id = tree::ROOT_ID;
    for (i, seg) in segs.into_iter().enumerate() {

        // Read node.
        if seg != b"" {
            node_id = match tree.get_child(node_id, seg) {
                Ok(n) => n,
                Err(_) => break,
            };
        }
        let node_locks = match tree.get_node(node_id) {
            Ok(n) => n,
            Err(_) => break,
        };

        for nl in node_locks {
            if i < last_seg && !nl.deep {
                continue
            }
            let m = submitted_tokens.iter().any(|t| &nl.token == t);
            if m {
                // fine, we hold this lock.
                holds_lock = true;
            } else {
                // exclusive locks are fatal.
                if !nl.shared {
                    return Err(nl.to_owned());
                }
                // remember first shared lock seen.
                if !shared_ok {
                    first_lock_seen.get_or_insert(nl);
                }
            }
        }

    }

    // return conflicting lock on error.
    if !holds_lock && first_lock_seen.is_some() {
        return Err(first_lock_seen.unwrap().to_owned());
    }

    Ok(())
}

// Find or create node.
fn get_or_create_path_node<'a>(tree: &'a mut Tree, path: &WebPath) -> &'a mut Vec<DavLock> {
    let path = path.as_bytes();
    let segs : Vec<&[u8]> = path.split(|&c| c == b'/').filter(|s| s.len() > 0).collect();

    let mut node_id = tree::ROOT_ID;
    for seg in segs.into_iter() {
        node_id = match tree.get_child(node_id, seg) {
            Ok(n) => n,
            Err(_) => {
                tree.add_child(node_id, seg.to_vec(), Vec::new(), false).unwrap()
            },
        };
    }
    tree.get_node_mut(node_id).unwrap()
}

// Find lock in path.
fn lookup_lock(tree: &Tree, path: &WebPath, token: &str) -> Option<u64> {

    let path = path.as_bytes();
    let segs : Vec<&[u8]> = path.split(|&c| c == b'/').filter(|s| s.len() > 0).collect();

    let mut node_id = tree::ROOT_ID;
    for seg in segs.into_iter() {
        node_id = match tree.get_child(node_id, seg) {
            Ok(n) => {
                let node = tree.get_node(n).unwrap();
                if node.iter().any(|n| n.token ==token) {
                    return Some(n);
                }
                n
            },
            Err(_) => return None,
        };
    }
    None
}

// Find node ID for path.
fn lookup_node(tree: &Tree, path: &WebPath) -> Option<u64> {

    let path = path.as_bytes();
    let segs : Vec<&[u8]> = path.split(|&c| c == b'/').filter(|s| s.len() > 0).collect();

    let mut node_id = tree::ROOT_ID;
    for seg in segs.into_iter() {
        node_id = match tree.get_child(node_id, seg) {
            Ok(n) => n,
            Err(_) => return None,
        };
    }
    Some(node_id)
}

// See if there are locks in any path below this collection.
fn check_locks_from_path(tree: &Tree, path: &WebPath, shared_ok: bool) -> Result<(), DavLock> {
    let node_id = match lookup_node(tree, path) {
        Some(id) => id,
        None => return Ok(()),
    };
    check_locks_from_node(tree, node_id, shared_ok)
}

// See if there are locks in any nodes below this node.
fn check_locks_from_node(tree: &Tree, node_id: u64, shared_ok: bool) -> Result<(), DavLock> {
    let node_locks = match tree.get_node(node_id) {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    for nl in node_locks {
        if !nl.shared || !shared_ok {
            return Err(nl.to_owned());
        }
    }
    if let Ok(children) = tree.get_children(node_id) {
        for (_, node_id) in children {
            if let Err(l) = check_locks_from_node(tree, node_id, shared_ok) {
                return Err(l);
            }
        }
    }
    Ok(())
}

// Find all locks in a path
fn list_locks(tree: &Tree, path: &WebPath) -> Vec<DavLock> {

    let path = path.as_bytes();
    let segs : Vec<&[u8]> = path.split(|&c| c == b'/').filter(|s| s.len() > 0).collect();

    let mut locks = Vec::new();

    let mut node_id = tree::ROOT_ID;
    if let Ok(node) = tree.get_node(node_id) {
        locks.extend_from_slice(node);
    }
    for seg in segs.into_iter() {
        node_id = match tree.get_child(node_id, seg) {
            Ok(n) => n,
            Err(_) => break,
        };
        if let Ok(node) = tree.get_node(node_id) {
            locks.extend_from_slice(node);
        }
    }
    locks
}
