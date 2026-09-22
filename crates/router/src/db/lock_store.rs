//! Slot-wide locks are independent of a particular contract (including empty slots).
//! Legacy pinned_instance values remain locks; individual instance pins stay separate.
use super::Db;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashSet;

pub(super) fn migrate(conn:&Connection)->Result<()> {
    let columns:Vec<String>=conn.prepare("PRAGMA table_info(slots)")?.query_map([],|r|r.get(1))?.collect::<rusqlite::Result<_>>()?;
    if !columns.iter().any(|c|c=="locked") {
        conn.execute_batch("ALTER TABLE slots ADD COLUMN locked INTEGER NOT NULL DEFAULT 0;
            UPDATE slots SET locked=1 WHERE pinned_instance IS NOT NULL;")?;
    }
    let columns:Vec<String>=conn.prepare("PRAGMA table_info(pending_operations)")?.query_map([],|r|r.get(1))?.collect::<rusqlite::Result<_>>()?;
    if !columns.iter().any(|c|c=="respect_lock") {
        // Unknown legacy origin: fail closed against a subsequently set lock.
        conn.execute_batch("ALTER TABLE pending_operations ADD COLUMN respect_lock INTEGER;")?;
    }
    Ok(())
}
impl Db {
    pub fn slot_locks(&self)->Result<HashSet<i64>> {
        let conn=self.0.lock().unwrap();
        let mut stmt=conn.prepare("SELECT id FROM slots WHERE locked<>0 OR pinned_instance IS NOT NULL")?;
        let rows=stmt.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn set_slot_locked(&self,slot:i64,locked:bool)->Result<bool> {
        let mut conn=self.0.lock().unwrap();let tx=conn.transaction()?;
        let (old,legacy):(bool,Option<i64>)=tx.query_row("SELECT locked<>0 OR pinned_instance IS NOT NULL,pinned_instance FROM slots WHERE id=?1",[slot],|r|Ok((r.get(0)?,r.get(1)?))).context("unknown slot")?;
        tx.execute("UPDATE slots SET locked=?2,pinned_instance=CASE WHEN ?2=0 THEN NULL ELSE pinned_instance END WHERE id=?1",params![slot,locked])?;
        if !locked {
            // Only undo the pin associated with an old slot lock, never unrelated pins.
            if let Some(id)=legacy {tx.execute("UPDATE instances SET pinned=0 WHERE vast_id=?1 AND slot_id=?2",params![id,slot])?;}
        }
        tx.commit()?;Ok(old!=locked)
    }
    pub fn pending_respects_lock(&self,id:i64)->Result<bool> {
        let value:Option<bool>=self.0.lock().unwrap().query_row("SELECT respect_lock FROM pending_operations WHERE instance_id=?1",[id],|r|r.get(0))?;
        Ok(value.unwrap_or(true))
    }
}
