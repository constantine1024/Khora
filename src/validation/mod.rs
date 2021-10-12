use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use crate::account::*;
use rayon::prelude::*;
use crate::transaction::*;
use std::convert::TryInto;
use std::iter::FromIterator;
use crate::bloom::BloomFile;
use rand::{thread_rng};
use sha3::{Digest, Sha3_512};
use ahash::AHasher;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::rename;
use std::io::Read;
use std::io::Write;
use std::hash::Hasher;
use serde::{Serialize, Deserialize};
use std::collections::{HashMap, HashSet, VecDeque};
use crate::constants::PEDERSEN_H;
use std::io::{Seek, SeekFrom, BufReader};//, BufWriter};


pub const NUMBER_OF_VALIDATORS: usize = 3;
pub const SIGNING_CUTOFF: usize = 2*NUMBER_OF_VALIDATORS/3;
pub const QUEUE_LENGTH: usize = 10;
pub const REPLACERATE: usize = 2;
pub const PUNISHMENT_FRACTION: u64 = 1000;

fn hash_to_scalar<T: Serialize> (message: &T) -> Scalar {
    let message = bincode::serialize(message).unwrap();
    let mut hasher = Sha3_512::new();
    hasher.update(&message);
    Scalar::from_hash(hasher)
} /* this is for testing purposes. it is used to check if 2 long messages are identicle */

#[derive(Default, Clone, Serialize, Deserialize, Eq, Hash, Debug)]
pub struct Syncedtx{
    pub stkout: Vec<u64>,
    pub stkin: Vec<(CompressedRistretto,u64)>,
    pub txout: Vec<OTAccount>, // they delete this part individually after they realize it's not for them
    pub tags: Vec<CompressedRistretto>,
    pub fees: u64,
}

impl PartialEq for Syncedtx {
    fn eq(&self, other: &Self) -> bool {
        self.stkout == other.stkout && self.stkin == other.stkin && self.txout == other.txout && self.tags == other.tags && self.fees == other.fees
    }
}

impl Syncedtx {
    pub fn from(txs: &Vec<PolynomialTransaction>)->Syncedtx {
        let stkout = txs.iter().filter_map(|x|
            if x.inputs.last() == Some(&1) {Some(x.inputs.par_chunks_exact(8).map(|x| u64::from_le_bytes(x.try_into().unwrap())).collect::<Vec<_>>())} else {None}
        ).flatten().collect::<Vec<u64>>();
        let stkin = txs.iter().map(|x|
            x.outputs.iter().filter_map(|y| 
                if let Ok(z) = stakereader_acc().read_ot(y) {Some((z.pk.compress(),u64::from_le_bytes(z.com.amount.unwrap().as_bytes()[..8].try_into().unwrap())))} else {None}
            ).collect::<Vec<_>>()
        ).flatten().collect::<Vec<(CompressedRistretto,u64)>>();
        let txout = txs.into_iter().map(|x|
            x.outputs.to_owned().into_iter().filter(|x| stakereader_acc().read_ot(x).is_err()).collect::<Vec<_>>()
        ).flatten().collect::<Vec<OTAccount>>();
        let tags = txs.iter().filter_map(|x|
            if x.inputs.last() != Some(&1) {Some(x.tags.clone())} else {None}
        ).flatten().collect::<Vec<CompressedRistretto>>();
        let fees = txs.iter().map(|x|x.fee).sum::<u64>();
        Syncedtx{stkout,stkin,txout,tags,fees}
    }
    pub fn to_sign(txs: &Vec<PolynomialTransaction>)->Vec<u8> {
        bincode::serialize(&Syncedtx::from(txs)).unwrap()
    }
}


#[derive(Default, Clone, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct MultiSignature{
    pub x: CompressedRistretto,
    pub y: Scalar,
    pub pk: Vec<u8>,
}

impl PartialEq for MultiSignature {
    fn eq(&self, other: &Self) -> bool {
        self.x == other.x && self.y == other.y && self.pk == other.pk
    }
}

impl MultiSignature {
    pub fn gen_group_x(key: &Scalar, nonce: &u64) -> CompressedRistretto {
        let mut s = Sha3_512::new();
        s.update(&key.as_bytes());
        s.update(nonce.to_le_bytes());
        let m = ((Scalar::from_hash(s))*PEDERSEN_H()).compress();
        m
    }
    pub fn sum_group_x<'a, I: IntoParallelRefIterator<'a, Item = &'a RistrettoPoint>>(x: &'a I) -> CompressedRistretto {
        x.par_iter().sum::<RistrettoPoint>().compress()
    }
    pub fn try_get_y(key: &Scalar, message: &Vec<u8>, xt: &CompressedRistretto, nonce: &u64) -> Scalar {
        let mut s = Sha3_512::new();
        s.update(&key.as_bytes());
        s.update(nonce.to_le_bytes());
        let r = Scalar::from_hash(s);

        let mut s = Sha3_512::new();
        s.update(&message);
        s.update(&xt.as_bytes());
        let e = Scalar::from_hash(s);
        let y = e*key+r;
        y
    }
    pub fn sum_group_y<'a, I: IntoParallelRefIterator<'a, Item = &'a Scalar>>(y: &'a I) -> Scalar {
        y.par_iter().sum()
    }
    pub fn verify_group(yt: &Scalar, xt: &CompressedRistretto, message: &Vec<u8>, who: &Vec<CompressedRistretto>) -> bool {
        let mut s = Sha3_512::new();
        s.update(&message);
        s.update(&xt.as_bytes());
        let e = Scalar::from_hash(s);
        (yt*PEDERSEN_H()) == (xt.decompress().unwrap() + e*who.into_par_iter().collect::<HashSet<_>>().into_par_iter().map(|x|x.decompress().unwrap()).sum::<RistrettoPoint>())
    }
}


#[derive(Default, Clone, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct ValidatorSignature{
    pub c: Scalar,
    pub r: Scalar,
    pub pk: u8,
}

impl PartialEq for ValidatorSignature {
    fn eq(&self, other: &Self) -> bool {
        self.c == other.c && self.r == other.r && self.pk == other.pk
    }
}

impl ValidatorSignature { // THIS IS NOT IN USE YET
    pub fn sign(key: &Scalar, message: &mut Sha3_512, location: &u8) -> ValidatorSignature {
        // let mut s = Sha3_512::new();
        // s.update(&message);
        let mut csprng = thread_rng();
        let a = Scalar::random(&mut csprng);
        message.update((a*PEDERSEN_H()).compress().to_bytes());
        let c = Scalar::from_hash(message.to_owned());
        ValidatorSignature{c, r: (a - c*key), pk: *location}
    }
    pub fn to_signature(&self, validator_pool: &Vec<u64>) -> Signature {
        Signature {
            c: self.c,
            r: self.r,
            pk: validator_pool[self.pk as usize],
        }
    }
}
#[derive(Default, Clone, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct Signature{
    pub c: Scalar,
    pub r: Scalar,
    pub pk: u64, // should i switch this to u8 and only validation squad is involved?
}

impl PartialEq for Signature {
    fn eq(&self, other: &Self) -> bool {
        self.c == other.c && self.r == other.r && self.pk == other.pk
    }
}

impl Signature {
    pub fn to_validator_signature(&self, validator_pool: &Vec<u64>) -> ValidatorSignature { // this hasn't actually been implimented in the blocks yet
        ValidatorSignature{
            c: self.c,
            r: self.r,
            pk: validator_pool.par_iter().enumerate().filter_map(|(i,&pk)| {
                if pk == self.pk {
                    Some(i)
                } else {
                    None
                }
            }).collect::<Vec<_>>()[0] as u8
        }
    }
    pub fn sign(key: &Scalar, message: &mut Sha3_512, location: &u64) -> Signature { // the inputs are the hashed messages you are checking for signatures on because it's faster for many messages.
        let mut csprng = thread_rng();
        let a = Scalar::random(&mut csprng);
        message.update((a*PEDERSEN_H()).compress().to_bytes());
        let c = Scalar::from_hash(message.to_owned());
        Signature{c, r: (a - c*key), pk: *location}
    }
    pub fn verify(&self, message: &mut Sha3_512, stkstate: &Vec<(CompressedRistretto,u64)>) -> bool { // the inputs are the hashed messages you are checking for signatures on because it's faster for many messages.
        if self.pk as usize >= stkstate.len() {return false}
        message.update((self.r*PEDERSEN_H() + self.c*stkstate[self.pk as usize].0.decompress().unwrap()).compress().to_bytes());
        self.c == Scalar::from_hash(message.to_owned())
    }

    pub fn sign_message(key: &Scalar, message: &Vec<u8>, location: &u64) -> Vec<u8> {
        let mut s = Sha3_512::new();
        s.update(&message); // impliment non block check stuff for signatures
        let mut csprng = thread_rng();
        let a = Scalar::random(&mut csprng);
        s.update((a*PEDERSEN_H()).compress().to_bytes());
        let c = Scalar::from_hash(s.to_owned());
        let mut out = c.as_bytes().to_vec();
        out.par_extend((a - c*key).as_bytes());
        out.par_extend(location.to_le_bytes());
        out.par_extend(message);
        out
    }
    pub fn recieve_signed_message(signed_message: &mut Vec<u8>, stkstate: &Vec<(CompressedRistretto,u64)>) -> Option<u64> {
        let sig = signed_message.par_drain(..72).collect::<Vec<_>>();
        let s = Signature{
            c: Scalar::from_bits(sig[..32].try_into().unwrap()),
            r: Scalar::from_bits(sig[32..64].try_into().unwrap()),
            pk: u64::from_le_bytes(sig[64..72].try_into().unwrap())
        };
        
        let mut h = Sha3_512::new();
        h.update(signed_message);
        if s.verify(&mut h, stkstate) {
            Some(s.pk)
        } else {
            None
        }
    }


    pub fn sign_message_nonced(key: &Scalar, message: &Vec<u8>, location: &u64, bnum: &u64) -> Vec<u8> {
        let mut s = Sha3_512::new();
        s.update(&message); // impliment non block check stuff for signatures
        s.update(bnum.to_le_bytes());
        let mut csprng = thread_rng();
        let a = Scalar::random(&mut csprng);
        s.update((a*PEDERSEN_H()).compress().to_bytes());
        let c = Scalar::from_hash(s.to_owned());
        let mut out = c.as_bytes().to_vec();
        out.par_extend((a - c*key).as_bytes());
        out.par_extend(location.to_le_bytes());
        out.par_extend(message);
        out
    }
    pub fn recieve_signed_message_nonced(signed_message: &mut Vec<u8>, stkstate: &Vec<(CompressedRistretto,u64)>, bnum: &u64) -> Option<u64> {
        if signed_message.len() < 72 {return None}
        let sig = signed_message.par_drain(..72).collect::<Vec<_>>();
        let s = Signature{
            c: Scalar::from_bits(sig[..32].try_into().unwrap()),
            r: Scalar::from_bits(sig[32..64].try_into().unwrap()),
            pk: u64::from_le_bytes(sig[64..72].try_into().unwrap())
        };
        
        let mut h = Sha3_512::new();
        h.update(signed_message);
        h.update(bnum.to_le_bytes());
        if s.verify(&mut h, stkstate) {
            Some(s.pk)
        } else {
            None
        }
    }



}



#[derive(Default, Clone, Eq, Serialize, Deserialize, Hash, Debug)]
pub struct NextBlock {
    pub emptyness: Option<MultiSignature>,
    pub validators: Option<Vec<Signature>>,
    pub leader: Signature,
    pub txs: Vec<PolynomialTransaction>,
    pub last_name: Vec<u8>,
    pub shards: Vec<u16>,
    pub bnum: u64,
}
impl PartialEq for NextBlock {
    fn eq(&self, other: &Self) -> bool {
        bincode::serialize(self).unwrap() == bincode::serialize(other).unwrap()
    }
}
impl NextBlock {
    pub fn valicreate(key: &Scalar, location: &u64, leader: &CompressedRistretto, txs: &Vec<PolynomialTransaction>, pool: &u16, bnum: &u64, last_name: &Vec<u8>, bloom: &BloomFile,/* _history: &Vec<OTAccount>,*/ stkstate: &Vec<(CompressedRistretto,u64)>) -> NextBlock {
        let stks = txs.par_iter().filter_map(|x| 
            if x.inputs.last() == Some(&1) {if x.verifystk(&stkstate).is_ok() {Some(x.to_owned())} else {None}} else {None}
        ).collect::<Vec<PolynomialTransaction>>(); /* i would use drain_filter but its unstable */
        let mut stks = stks.par_iter().enumerate().filter(|(i,x)| {
            x.inputs.par_chunks_exact(8).map(|x| u64::from_le_bytes(x.try_into().unwrap())).collect::<Vec<_>>().par_iter().all(|&x|
                !stks[..*i].par_iter().flat_map(|x| x.inputs.par_chunks_exact(8).map(|x| u64::from_le_bytes(x.try_into().unwrap())).collect::<Vec<_>>()).collect::<Vec<u64>>()
                .contains(&x)
            )
        }).map(|x| x.1.to_owned()).collect::<Vec<_>>();
        let txs = txs.into_par_iter().filter_map(|x| 
            if x.inputs.last() == Some(&0) {Some(x.to_owned())} else {None}
        ).collect::<Vec<PolynomialTransaction>>();
        
        let mut txs =
            txs.par_iter().enumerate().filter_map(|(i,x)| {
                if 
                x.tags.par_iter().all(|&x| !txs[..i].iter().flat_map(|x| x.tags.clone()).collect::<HashSet<CompressedRistretto>>().contains(&x))
                &&
                x.tags.iter().all(|y| {!bloom.contains(&y.to_bytes())})
                &&
                x.tags.len() == x.tags.iter().collect::<HashSet<_>>().len()
                &&
                x.verify().is_ok()
                {
                    Some(x.to_owned())
                }
                else {None}
        }).collect::<Vec<PolynomialTransaction>>();
        txs.append(&mut stks);


        let m = vec![leader.to_bytes().to_vec(),bincode::serialize(&vec![pool]).unwrap(),Syncedtx::to_sign(&txs),bnum.to_le_bytes().to_vec(), last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
        let mut s = Sha3_512::new();
        s.update(&m);
        NextBlock {
            emptyness: None,
            validators: None,
            leader: Signature::sign(&key,&mut s,&location),
            txs: txs.to_owned(),
            last_name: last_name.to_owned(),
            shards: vec![*pool],
            bnum: *bnum,
        }
    }
    pub fn finish(key: &Scalar, location: &u64, sigs: &Vec<NextBlock>, validator_pool: &Vec<u64>, pool: &u16, bnum: &u64, last_name: &Vec<u8>, stkstate: &Vec<(CompressedRistretto,u64)>) -> NextBlock { // <----do i need to reference previous block explicitly?
        let leader = (key*PEDERSEN_H()).compress().as_bytes().to_vec();
        let mut sigs = sigs.into_par_iter().filter(|x| !validator_pool.into_par_iter().all(|y| x.leader.pk != *y)).map(|x| x.to_owned()).collect::<Vec<NextBlock>>();
        let mut sigfinale: Vec<NextBlock>;
        for _ in 0..=(sigs.len() - SIGNING_CUTOFF) {
            let b = sigs.pop().unwrap();
            sigfinale = sigs.par_iter().filter(|x| if let (Ok(z),Ok(y)) = (bincode::serialize(&x.txs),bincode::serialize(&b.txs)) {z==y} else {false}).map(|x| x.to_owned()).collect::<Vec<NextBlock>>();
            if validator_pool.par_iter().filter(|x| !sigfinale.par_iter().all(|y| x.to_owned() != &y.leader.pk)).count() >= SIGNING_CUTOFF {
                sigfinale.push(b);
                println!("they agree on tx in block validation");
                let sigfinale = sigfinale.par_iter().enumerate().filter_map(|(i,x)| if sigs[..i].par_iter().all(|y| x.leader.pk != y.leader.pk) {Some(x.to_owned())} else {None}).collect::<Vec<NextBlock>>();
                let m = vec![leader.clone(),bincode::serialize(&vec![pool]).unwrap(),Syncedtx::to_sign(&sigfinale[0].txs),bnum.to_le_bytes().to_vec(), last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
                let mut s = Sha3_512::new();
                s.update(&m);
                let sigfinale = sigfinale.into_par_iter().filter(|x| Signature::verify(&x.leader, &mut s.clone(),&stkstate)).map(|x| x.to_owned()).collect::<Vec<NextBlock>>();
                let signers = sigfinale.clone();
                let sigfinale = sigfinale.into_par_iter().enumerate().filter_map(|(e,x)|
                    if signers[..e].par_iter().all(|y| y.leader.pk != x.leader.pk) {
                        Some(x)
                    } else {
                        None
                    }
                ).collect::<Vec<_>>();
                let sigs = sigfinale.par_iter().map(|x| x.leader.to_owned()).collect::<Vec<Signature>>();
                let mut s = Sha3_512::new();
                s.update(&bincode::serialize(&sigs).unwrap().to_vec());
                let c = s.finalize();
                let m = vec![bincode::serialize(&vec![pool]).unwrap(),bnum.to_le_bytes().to_vec(),last_name.clone(),c.to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
                let mut s = Sha3_512::new();
                s.update(&m);
                let leader = Signature::sign(&key, &mut s,&location);
                if validator_pool.par_iter().filter(|x| !sigfinale.par_iter().all(|y| x.to_owned() != &y.leader.pk)).count() > SIGNING_CUTOFF {
                    return NextBlock{emptyness: None, validators: Some(sigs), leader, txs: sigfinale[0].txs.to_owned(), last_name: last_name.to_owned(), shards: vec![*pool], bnum: bnum.to_owned()}
                } else {
                    print!("not enough sigs... ");
                    break
                }
            }
        }
        println!("failed to make block :(");
        return NextBlock::default()
    }
    pub fn valimerge(key: &Scalar, location: &u64, leader: &CompressedRistretto, blks: &Vec<NextBlock>, val_pools: &Vec<Vec<u64>>, bnum: &u64, last_name: &Vec<u8>, stkstate: &Vec<(CompressedRistretto,u64)>, _mypoolnum: &u16) -> Signature {
        // WARNING:: MUST MAKE SURE blks[0] IS THE ONE YOU MADE YOURSELF
        
        
        let mut blks: Vec<NextBlock> = blks.par_iter().zip(val_pools).filter_map(|(x,y)| if x.verify(&y,&stkstate).is_ok() {Some(x.to_owned())} else {None}).collect();
        let mut blk = blks.remove(0); // their own shard should be this one! (main shard should be contributing as a shard while waiting)
        let mut tags = blk.txs.par_iter().map(|x| x.tags.clone()).flatten().collect::<HashSet<Tag>>();
        for mut b in blks {
            if b.bnum == *bnum {
                b.txs = b.txs.into_par_iter().filter(|t| {
                    // tags.is_disjoint(&HashSet::from_par_iter(t.tags.par_iter().cloned()))
                    t.tags.par_iter().all(|x| !tags.contains(x))
                }).collect::<Vec<PolynomialTransaction>>();
                blk.txs.par_extend(b.txs.clone());
                let x = b.txs.len();
                tags = tags.union(&b.txs.into_par_iter().map(|x| x.tags).flatten().collect::<HashSet<Tag>>()).map(|&x| x).collect::<HashSet<CompressedRistretto>>();
                if x > 63 {
                    blk.shards.par_extend(b.shards);
                }
            }
        }


        let m = vec![leader.to_bytes().to_vec(),bincode::serialize(&blk.shards).unwrap(),Syncedtx::to_sign(&blk.txs),bnum.to_le_bytes().to_vec(), last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
        let mut s = Sha3_512::new();
        s.update(&m);
        Signature::sign(&key,&mut s, &location)
    }
    pub fn finishmerge(key: &Scalar, location: &u64, sigs: &Vec<Signature>, blks: &Vec<NextBlock>, val_pools: &Vec<Vec<u64>>, headpool: &Vec<u64>, bnum: &u64, last_name: &Vec<u8>, stkstate: &Vec<(CompressedRistretto,u64)>, _mypoolnum: &u16) -> NextBlock {
        let headpool = headpool.into_par_iter().map(|x|stkstate[*x as usize].0).collect::<Vec<CompressedRistretto>>();
        let mut blks: Vec<NextBlock> = blks.par_iter().zip(val_pools).filter_map(|(x,y)| if x.verify(&y, &stkstate).is_ok() {Some(x.to_owned())} else {None}).collect();
        let mut blk = blks.remove(0);
        
        let mut tags = blk.txs.par_iter().map(|x| x.tags.clone()).flatten().collect::<HashSet<Tag>>();
        for mut b in blks.into_iter() {
            b.txs = b.txs.into_par_iter().filter(|t| {
                t.tags.par_iter().all(|x| !tags.contains(x))
            }).collect::<Vec<PolynomialTransaction>>();
            blk.txs.par_extend(b.txs.clone());
            let x = b.txs.len();
            tags = tags.union(&b.txs.into_par_iter().map(|x| x.tags).flatten().collect::<HashSet<Tag>>()).map(|&x| x).collect::<HashSet<CompressedRistretto>>();
            // println!("tx: {}",x);
            if x > 63 {
                blk.shards.par_extend(b.shards);
            }
        }
        
        let leader = (key*PEDERSEN_H()).compress().as_bytes().to_vec();
        let m = vec![leader.clone(),bincode::serialize(&blk.shards).unwrap(),Syncedtx::to_sign(&blk.txs),bnum.to_le_bytes().to_vec(),last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
        let mut s = Sha3_512::new();
        s.update(&m);
        let sigs = sigs.into_par_iter().filter(|x|
            Signature::verify(x, &mut s.clone(), stkstate)
        ).collect::<Vec<&Signature>>();
        let sigs = sigs.into_par_iter().filter(|x| !headpool.clone().into_par_iter().all(|y| stkstate[x.pk as usize].0 != y)).collect::<Vec<&Signature>>();
        let sigcopy = sigs.clone();
        let sigs = sigs.into_par_iter().enumerate().filter_map(|(i,x)| if sigcopy[..i].par_iter().all(|y| x.pk != y.pk) {Some(x.to_owned())} else {None}).collect::<Vec<Signature>>();
        let mut s = Sha3_512::new();
        s.update(&bincode::serialize(&sigs).unwrap().to_vec());
        let c = s.finalize();

        let m = vec![bincode::serialize(&blk.shards).unwrap(),bnum.to_le_bytes().to_vec(), last_name.clone(),c.to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
        let mut s = Sha3_512::new();
        s.update(&m);
        let leader = Signature::sign(&key, &mut s, &location);
        NextBlock{emptyness: None, validators: Some(sigs), leader, txs: blk.txs, last_name: last_name.clone(), shards: blk.shards, bnum: bnum.to_owned()}
    }
    pub fn verify(&self, validator_pool: &Vec<u64>, stkstate: &Vec<(CompressedRistretto,u64)>) -> Result<bool, &'static str> {
        if let Some(validators) = self.validators.clone() {
            if self.emptyness.is_some() {
                return Err("both the validators and the emptyness are some")
            }
            let mut s = Sha3_512::new();
            s.update(&bincode::serialize(&validators).unwrap().to_vec());
            let c = s.finalize();
            let m = vec![bincode::serialize(&self.shards).unwrap(),self.bnum.to_le_bytes().to_vec(), self.last_name.clone(),c.to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut s = Sha3_512::new();
            s.update(&m);

            if !self.leader.verify(&mut s, &stkstate) {
                return Err("leader is fake")
            }
            let m = vec![stkstate[self.leader.pk as usize].0.as_bytes().to_vec().clone(),bincode::serialize(&self.shards).unwrap(),Syncedtx::to_sign(&self.txs),self.bnum.to_le_bytes().to_vec(), self.last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut h = Sha3_512::new();
            h.update(&m);
            if !validators.par_iter().all(|x| x.verify(&mut h.clone(), &stkstate)) {
                return Err("at least 1 validator is fake")
            }
            if !validators.par_iter().all(|x| !validator_pool.par_iter().all(|y| x.pk != *y)) {
                return Err("at least 1 validator is not in the pool")
            }
            if validator_pool.par_iter().filter(|x| !validators.par_iter().all(|y| x.to_owned() != &y.pk)).count() <= SIGNING_CUTOFF {
                return Err("there aren't enough validators")
            }
            let x = validators.par_iter().map(|x| x.pk).collect::<Vec<u64>>();
            if !x.clone().par_iter().enumerate().all(|(i,y)| x[..i].par_iter().all(|z| y != z)) {
                return Err("there's multiple signatures from the same validator")
            }
        } else if let Some(emptyness) = self.emptyness.clone() {
            if self.txs.len() > 0 {
                return Err("the block isn't empty!")
            }
            if emptyness.pk.len() != emptyness.pk.par_iter().collect::<HashSet<_>>().len() {
                return Err("someone failed to sign twice as the same validator")
            }
            let who = validator_pool.into_par_iter().filter_map(|&x|
                if emptyness.pk.par_iter().all(|&y| validator_pool[y as usize]!=x) {
                    Some(stkstate[x as usize].0)
                } else {
                    None
                }
            ).collect::<Vec<_>>();
            if who.len() <= 2*NUMBER_OF_VALIDATORS/3 {
                return Err("there's not enough validators for the empty block")
            }
            if self.leader.pk as usize >= stkstate.len() {
                return Err("you're probably behind on blocks")
            }
            let mut m = stkstate[self.leader.pk as usize].0.as_bytes().to_vec();
            m.extend(&self.last_name);
            m.extend(&self.shards[0].to_le_bytes().to_vec());
            if !MultiSignature::verify_group(&emptyness.y,&emptyness.x,&m,&who) {
                return Err("multisignature can not be verified")
            }
            let m = vec![self.shards[0].to_le_bytes().to_vec(),self.bnum.to_le_bytes().to_vec(),self.last_name.clone(),bincode::serialize(&self.emptyness).unwrap().to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut s = Sha3_512::new();
            s.update(&m);
            if !self.leader.verify(&mut s,&stkstate) {
                return Err("there's a problem with the leader's multisignature group signature")
            }
        } else {
            return Err("both the validators and the emptyness are none")
        }
        return Ok(true)
    }
    pub fn pay_all_empty(shard: &usize, comittee: &Vec<Vec<usize>>, valinfo: &mut Vec<(CompressedRistretto,u64)>, reward: f64) {
        let winners = comittee[*shard].iter();
        let inflation = (reward/winners.len() as f64) as u64;
        for &i in winners {
            valinfo[i].1 += inflation;
        }
    }
    pub fn pay_self_empty(shard: &usize, comittee: &Vec<Vec<usize>>, mine: &mut Vec<[u64;2]>, reward: f64) -> bool {

        let winners = comittee[*shard].iter();
        let inflation = (reward/winners.len() as f64) as u64;
        let changed = std::sync::Arc::new(std::sync::Mutex::new(false));
        for &i in winners {
            mine.par_iter_mut().for_each(|x| if x[0] == i as u64 {*changed.lock().unwrap() = true; x[1] += inflation;});
        }
        let changed = *changed.lock().unwrap();
        changed
    }
    pub fn tolightning(&self) -> LightningSyncBlock {
        LightningSyncBlock {
            emptyness: self.emptyness.to_owned(),
            validators: self.validators.to_owned(),
            leader: self.leader.to_owned(),
            info: Syncedtx::from(&self.txs),
            shards: self.shards.to_owned(),
            bnum: self.bnum.to_owned(),
            last_name: self.last_name.to_owned(),
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize, Hash, Debug)]
pub struct LightningSyncBlock {
    pub emptyness: Option<MultiSignature>,
    pub validators: Option<Vec<Signature>>,
    pub leader: Signature,
    pub info: Syncedtx,
    pub shards: Vec<u16>,
    pub bnum: u64,
    pub last_name: Vec<u8>,
}
impl LightningSyncBlock {
    pub fn verify_multithread(&self, validator_pool: &Vec<u64>, stkstate: &Vec<(CompressedRistretto,u64)>) -> Result<bool, &'static str> {
        if let Some(validators) = self.validators.clone() {
            if self.emptyness.is_some() {
                return Err("both the validators and the emptyness are some")
            }
            let mut s = Sha3_512::new();
            s.update(&bincode::serialize(&validators).unwrap().to_vec());
            let c = s.finalize();
            let m = vec![bincode::serialize(&self.shards).unwrap(),self.bnum.to_le_bytes().to_vec(), self.last_name.clone(),c.to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut h = Sha3_512::new();
            h.update(&m);
            if !self.leader.verify(&mut h, &stkstate) {
                return Err("leader is fake")
            }
            let m = vec![stkstate[self.leader.pk as usize].0.as_bytes().to_vec().clone(), bincode::serialize(&self.shards).unwrap(), bincode::serialize(&self.info).unwrap(), self.bnum.to_le_bytes().to_vec(), self.last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut h = Sha3_512::new();
            h.update(&m);
            if !validators.par_iter().all(|x| x.verify(&mut h.clone(), &stkstate)) {
                return Err("at least 1 validator is fake")
            }
            if !validators.par_iter().all(|x| !validator_pool.into_par_iter().all(|y| x.pk != *y)) {
                return Err("at least 1 validator is not in the pool")
            }
            if validator_pool.par_iter().filter(|x| !validators.par_iter().all(|y| x.to_owned() != &y.pk)).count() < SIGNING_CUTOFF {
                return Err("there aren't enough validators")
            }
            let x = validators.par_iter().map(|x| x.pk).collect::<Vec<u64>>();
            if !x.clone().par_iter().enumerate().all(|(i,y)| x.clone()[..i].into_par_iter().all(|z| y != z)) {
                return Err("there's multiple signatures from the same validator")
            }
        } else if let Some(emptyness) = self.emptyness.clone() {
            if self.info.tags.len() > 0 { //this is neccesary so that you cant just add transactions that no one signed off on (unless I also have them sign an empty vector)
                return Err("the block isn't empty!") // i should allow multisignatures for full blocks??
            }
            if emptyness.pk.len() != emptyness.pk.par_iter().collect::<HashSet<_>>().len() {
                return Err("someone failed to sign twice as the same validator")
            }
            let who = validator_pool.into_par_iter().filter_map(|&x|
                if emptyness.pk.par_iter().all(|&y| validator_pool[y as usize]!=x) {
                    Some(stkstate[x as usize].0)
                } else { // may need to add more checks here
                    None
                }
            ).collect::<Vec<_>>();
            if who.len() < SIGNING_CUTOFF {
                return Err("there's not enough validators for the empty block")
            }
            let mut m = stkstate[self.leader.pk as usize].0.as_bytes().to_vec();
            m.extend(&self.last_name);
            m.extend(&self.shards[0].to_le_bytes().to_vec());
            if !MultiSignature::verify_group(&emptyness.y,&emptyness.x,&m,&who) {
                return Err("there's a problem with the multisignature")
            }
            let m = vec![self.shards[0].to_le_bytes().to_vec(),self.bnum.to_le_bytes().to_vec(),self.last_name.clone(),bincode::serialize(&self.emptyness).unwrap().to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut s = Sha3_512::new();
            s.update(&m);
            if !self.leader.verify(&mut s,&stkstate) {
                return Err("there's a problem with the leader's multisignature group signature")
            }
        } else {
            return Err("both the validators and the emptyness are none")
        }
        return Ok(true)
    }
    pub fn verify(&self, validator_pool: &Vec<u64>, stkstate: &Vec<(CompressedRistretto,u64)>) -> Result<bool, &'static str> {
        if let Some(validators) = self.validators.clone() {
            if self.emptyness.is_some() {
                return Err("both the validators and the emptyness are some")
            }
            let mut s = Sha3_512::new();
            s.update(&bincode::serialize(&validators).unwrap().to_vec());
            let c = s.finalize();
            let m = vec![bincode::serialize(&self.shards).unwrap(),self.bnum.to_le_bytes().to_vec(), self.last_name.clone(),c.to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut h = Sha3_512::new();
            h.update(&m);
            if !self.leader.verify(&mut h, &stkstate) {
                return Err("leader is fake")
            }
            let m = vec![stkstate[self.leader.pk as usize].0.as_bytes().to_vec().clone(), bincode::serialize(&self.shards).unwrap(), bincode::serialize(&self.info).unwrap(), self.bnum.to_le_bytes().to_vec(), self.last_name.clone()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut h = Sha3_512::new();
            h.update(&m);
            if !validators.iter().all(|x| x.verify(&mut h.clone(), &stkstate)) {
                return Err("at least 1 validator is fake")
            }
            if !validators.iter().all(|x| !validator_pool.into_iter().all(|y| x.pk != *y)) {
                return Err("at least 1 validator is not in the pool")
            }
            if validator_pool.iter().filter(|x| !validators.iter().all(|y| x.to_owned() != &y.pk)).count() < SIGNING_CUTOFF {
                return Err("there aren't enough validators")
            }
            let x = validators.iter().map(|x| x.pk).collect::<Vec<u64>>();
            if !x.clone().iter().enumerate().all(|(i,y)| x.clone()[..i].into_iter().all(|z| y != z)) {
                return Err("there's multiple signatures from the same validator")
            }
        } else if let Some(emptyness) = self.emptyness.clone() {
            if self.info.tags.len() > 0 { //this is neccesary so that you cant just add transactions that no one signed off on (unless I also have them sign an empty vector)
                return Err("the block isn't empty!") // i should allow multisignatures for full blocks??
            }
            if emptyness.pk.len() != emptyness.pk.iter().collect::<HashSet<_>>().len() {
                return Err("someone failed to sign twice as the same validator")
            }
            let who = validator_pool.into_iter().filter_map(|&x|
                if emptyness.pk.iter().all(|&y| validator_pool[y as usize]!=x) {
                    Some(stkstate[x as usize].0)
                } else { // may need to add more checks here
                    None
                }
            ).collect::<Vec<_>>();
            if who.len() < SIGNING_CUTOFF {
                return Err("there's not enough validators for the empty block")
            }
            let mut m = stkstate[self.leader.pk as usize].0.as_bytes().to_vec();
            m.extend(&self.last_name);
            m.extend(&self.shards[0].to_le_bytes().to_vec());
            if !MultiSignature::verify_group(&emptyness.y,&emptyness.x,&m,&who) {
                return Err("there's a problem with the multisignature")
            }
            let m = vec![self.shards[0].to_le_bytes().to_vec(),self.bnum.to_le_bytes().to_vec(),self.last_name.clone(),bincode::serialize(&self.emptyness).unwrap().to_vec()].into_par_iter().flatten().collect::<Vec<u8>>();
            let mut s = Sha3_512::new();
            s.update(&m);
            if !self.leader.verify(&mut s,&stkstate) {
                return Err("there's a problem with the leader's multisignature group signature")
            }
        } else {
            return Err("both the validators and the emptyness are none")
        }
        return Ok(true)
    }
    pub fn scan_as_noone(&self, valinfo: &mut Vec<(CompressedRistretto,u64)>, queue: &mut Vec<VecDeque<usize>>, exitqueue: &mut Vec<VecDeque<usize>>, comittee: &mut Vec<Vec<usize>>, reward: f64, save_history: bool) {
        if save_history {History::append(&self.info.txout)};

        let winners: Vec<usize>;
        let masochists: Vec<usize>;
        let lucky: Vec<usize>;
        let feelovers: Vec<usize>;
        if let Some(x) = self.validators.clone() {
            let x = x.iter().map(|x| x.pk as usize).collect::<HashSet<_>>();

            winners = comittee[self.shards[0] as usize].iter().filter(|&y| x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            masochists = comittee[self.shards[0] as usize].iter().filter(|&y| !x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            if self.shards.len() > 1 {
                feelovers = self.shards[1..].iter().map(|x| comittee[*x as usize].clone()).flatten().chain(winners.clone()).collect::<Vec<_>>();
            } else {
                feelovers = winners.clone();
            }
            lucky = comittee[*self.shards.iter().max().unwrap() as usize + 1].clone();
        } else {
            let x = self.emptyness.clone().unwrap().pk.par_iter().map(|x| *x as usize).collect::<HashSet<_>>();

            winners = comittee[self.shards[0] as usize].par_iter().filter(|&y| !x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            masochists = comittee[self.shards[0] as usize].par_iter().filter(|&y| x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            lucky = comittee[*self.shards.iter().max().unwrap() as usize + 1].clone();
            feelovers = winners.clone();
        }
        let fees = self.info.fees/(feelovers.len() as u64);
        let inflation = (reward/winners.len() as f64) as u64;


        for i in winners {
            valinfo[i].1 += inflation;
        }
        for i in feelovers {
            valinfo[i].1 += fees;
        }
        let mut punishments = 0u64;
        for i in masochists {
            punishments += valinfo[i].1/PUNISHMENT_FRACTION;
            valinfo[i].1 -= valinfo[i].1/PUNISHMENT_FRACTION;
        }
        punishments = punishments/lucky.len() as u64;
        for i in lucky {
            valinfo[i].1 += punishments;
        }




        for x in self.info.stkout.iter().rev() {
            valinfo.remove(*x as usize);
            *queue = queue.into_par_iter().map(|y| {
                let z = y.into_par_iter().filter_map(|z|
                    if *z > *x as usize {Some(*z - 1)}
                    else if *z == *x as usize {None}
                    else {Some(*z)}
                ).collect::<VecDeque<_>>();
                if z.len() == 0 {
                    VecDeque::from_iter([0usize])
                } else {
                    z
                }
            }).collect::<Vec<_>>();
            *exitqueue = exitqueue.into_par_iter().map(|y| {
                let z = y.into_par_iter().filter_map(|z|
                    if *z > *x as usize {Some(*z - 1)}
                    else if *z == *x as usize {None}
                    else {Some(*z)}
                ).collect::<VecDeque<_>>();
                if z.len() == 0 {
                    VecDeque::from_iter([0usize])
                } else {
                    z
                }
            }).collect::<Vec<_>>();
            *comittee = comittee.into_par_iter().map(|y| {
                let z = y.into_par_iter().filter_map(|z|
                    if *z > *x as usize {Some(*z - 1)}
                    else if *z == *x as usize {None}
                    else {Some(*z)}
                ).collect::<Vec<_>>();
                if z.len() == 0 {
                    vec![0usize]
                } else {
                    z
                }
            }).collect::<Vec<_>>();
        }

        queue.par_iter_mut().for_each(|x| {
            let mut s = Sha3_512::new();
            s.update(&bincode::serialize(&x).unwrap());
            let mut v = Scalar::from_hash(s.clone()).as_bytes().to_vec();
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            let mut y = (0..QUEUE_LENGTH-x.len()).map(|i| x[v[i] as usize%x.len()]).collect::<VecDeque<usize>>();
            x.append(&mut y);
        });
        exitqueue.par_iter_mut().for_each(|x| {
            let mut s = Sha3_512::new();
            s.update(&bincode::serialize(&x).unwrap());
            let mut v = Scalar::from_hash(s.clone()).as_bytes().to_vec();
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            let mut y = (0..QUEUE_LENGTH-x.len()).map(|i| x[v[i] as usize%x.len()]).collect::<VecDeque<usize>>();
            x.append(&mut y);
        });
        comittee.par_iter_mut().for_each(|x| {
            let mut s = Sha3_512::new();
            s.update(&bincode::serialize(&x).unwrap());
            let mut v = Scalar::from_hash(s.clone()).as_bytes().to_vec();
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            s.update(&bincode::serialize(&x).unwrap());
            v.append(&mut Scalar::from_hash(s.clone()).as_bytes().to_vec());
            let mut y = (0..NUMBER_OF_VALIDATORS-x.len()).map(|i| x[v[i] as usize%x.len()]).collect::<Vec<usize>>();
            x.append(&mut y);
        });


        valinfo.extend(&self.info.stkin);


    }
    pub fn scan(&self, me: &Account, mine: &mut HashMap<u64,OTAccount>, height: &mut u64, alltagsever: &mut Vec<CompressedRistretto>) -> bool {
        let mut imtrue = true;
        let newmine = self.info.txout.iter().enumerate().filter_map(|(i,x)| if let Ok(y) = me.receive_ot(x) {imtrue = false; Some((i as u64+*height,y))} else {None}).collect::<Vec<(u64,OTAccount)>>();
        let newtags = newmine.iter().map(|x|x.1.tag.unwrap()).collect::<Vec<CompressedRistretto>>();
        if !newtags.par_iter().all(|x| alltagsever.par_iter().all(|y|y!=x)) {
            println!("you got burnt (someone sent you faerie gold!)"); // i want this in a seperate function
        }
        alltagsever.extend(&newtags);

        *mine = mine.into_iter().filter_map(|(j,a)| if self.info.tags.par_iter().all(|x| x != &a.tag.unwrap()) {imtrue = false; Some((*j,a.clone()))} else {None} ).collect::<HashMap<u64,OTAccount>>();
        *height += self.info.txout.len() as u64;
        mine.extend(newmine);

        imtrue
    }
    pub fn scanstk(&self, me: &Account, mine: &mut Vec<[u64;2]>, height: &mut u64, comittee: &Vec<Vec<usize>>, reward: f64, valinfo: &Vec<(CompressedRistretto,u64)>) -> bool {

        let winners: Vec<usize>;
        let masochists: Vec<usize>;
        let lucky: Vec<usize>;
        let feelovers: Vec<usize>;
        if let Some(x) = self.validators.clone() {
            let x = x.iter().map(|x| x.pk as usize).collect::<HashSet<_>>();

            winners = comittee[self.shards[0] as usize].iter().filter(|&y| x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            masochists = comittee[self.shards[0] as usize].iter().filter(|&y| !x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            if self.shards.len() > 1 {
                feelovers = self.shards[1..].iter().map(|x| comittee[*x as usize].clone()).flatten().chain(winners.clone()).collect::<Vec<_>>();
            } else {
                feelovers = winners.clone();
            }
            lucky = comittee[*self.shards.iter().max().unwrap() as usize + 1].clone();
        } else {
            let x = self.emptyness.clone().unwrap().pk.iter().map(|x| *x as usize).collect::<HashSet<_>>();

            winners = comittee[self.shards[0] as usize].iter().filter(|&y| !x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            masochists = comittee[self.shards[0] as usize].iter().filter(|&y| x.contains(y)).map(|x| *x).collect::<Vec<_>>();
            lucky = comittee[*self.shards.iter().max().unwrap() as usize + 1].clone();
            feelovers = winners.clone();
        }
        let fees = self.info.fees/(feelovers.len() as u64);
        let inflation = (reward/winners.len() as f64) as u64;

        let changed = std::sync::Arc::new(std::sync::RwLock::new(false));
        for i in winners {
            mine.iter_mut().for_each(|x| if x[0] == i as u64 {*changed.write().unwrap() = true; x[1] += inflation;});
        }
        for i in feelovers {
            mine.iter_mut().for_each(|x| if x[0] == i as u64 {*changed.write().unwrap() = true; x[1] += fees;});
        }
        let mut punishments = 0u64;
        for i in masochists {
            punishments += valinfo[i].1/PUNISHMENT_FRACTION;
            mine.iter_mut().for_each(|x| if x[0] == i as u64 {*changed.write().unwrap() = true; x[1] -= valinfo[i].1/PUNISHMENT_FRACTION;});
        }
        punishments = punishments/lucky.len() as u64;
        for i in lucky {
            mine.iter_mut().for_each(|x| if x[0] == i as u64 {*changed.write().unwrap() = true; x[1] += punishments;});
        }


    



        for (i,m) in mine.clone().iter().enumerate().rev() {
            for v in self.info.stkout.iter() {
                if m[0] == *v {
                    *changed.write().unwrap() = true;
                    mine.remove(i as usize);
                }
            }
        }
        for (i,m) in mine.clone().iter().enumerate().rev() {
            for n in self.info.stkout.iter() {
                if *n < m[0] {
                    *changed.write().unwrap() = true;
                    mine[i][0] -= 1;
                }
                else {
                    break;
                }
            }
        }
        *height -= self.info.stkout.len() as u64;
        // println!("-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:-:\n{:?}",mine);
        
        let stkcr = me.stake_acc().derive_stk_ot(&Scalar::one()).pk.compress();
        mine.extend(self.info.stkin.iter().enumerate().filter_map(|(i,x)| if stkcr == x.0 {Some([i as u64+*height,x.1])} else {None}).collect::<Vec<[u64;2]>>());
        *height += self.info.stkin.len() as u64;

        let changed = *changed.read().unwrap();
        changed

    }
    pub fn update_bloom(&self,bloom:&BloomFile,parallel:&bool) {
        if *parallel {
            self.info.tags.par_iter().for_each(|x| bloom.insert(&x.as_bytes()));
        } else {
            self.info.tags.iter().for_each(|x| bloom.insert(&x.as_bytes()));
        }
    }
    pub fn save_history_to_ram(&self, history: &mut Vec<OTAccount>) {
        history.extend(self.info.txout.clone());
    }




}


/// selects the stakers who get to validate the queue and exit_queue
pub fn select_stakers(block: &Vec<u8>, bnum: &u64, shard: &u128, queue: &mut VecDeque<usize>, exitqueue: &mut VecDeque<usize>, comittee: &mut Vec<usize>, stkstate: &Vec<(CompressedRistretto,u64)>) {
    let (_pool,y): (Vec<CompressedRistretto>,Vec<u128>) = stkstate.into_iter().map(|(x,y)| (x.to_owned(),*y as u128)).unzip();
    let tot_stk: u128 = y.iter().sum(); /* initial queue will be 0 for all non0 shards... */

    let bnum = bnum.to_le_bytes();
    let mut s = AHasher::new_with_keys(0, *shard);
    s.write(&block);
    s.write(&bnum);
    let mut winner = (0..REPLACERATE).collect::<Vec<usize>>().iter().map(|x| {
        let mut s = s.clone();
        s.write(&x.to_le_bytes()[..]);
        let c = s.finish() as u128;
        // println!("unmoded winner:    {}",c);
        let mut staker = (c%tot_stk) as i128;
        // println!("winner:            {}",staker);
        let mut w = 0;
        for (i,&j) in y.iter().enumerate() {
            staker -= j as i128;
            if staker <= 0
                {w = i; break;}
        };
        w
    }).collect::<VecDeque<usize>>();
    queue.append(&mut winner); // need to hardcode initial state
    let winner = queue.par_drain(..REPLACERATE).collect::<Vec<usize>>();

    let mut s = AHasher::new_with_keys(1, *shard);
    s.write(&block);
    s.write(&bnum);
    let mut loser = (0..REPLACERATE).collect::<Vec<usize>>().iter().map(|x| {
        let mut s = s.clone();
        s.write(&x.to_le_bytes()[..]);
        let c = s.finish() as usize;
        c%NUMBER_OF_VALIDATORS
    }).collect::<VecDeque<usize>>();
    exitqueue.append(&mut loser);
    let loser = exitqueue.par_drain(..REPLACERATE).collect::<Vec<usize>>();

    for (i,j) in loser.iter().enumerate() {
        comittee[*j] = winner[i];
    }
}








pub struct History {}

static FILE_NAME: &str = "history";

/// this represents a file that saves the public keys and commitments of all the OTAccounts that have appeared on the block chain (it is used to verify transactions and generate rings)
impl History {
    /// Create the file
    pub fn initialize() {
        File::create(FILE_NAME).unwrap();
    }

    /// Get the information on the OTAccount at height location as compressed Ristrettos
    pub fn get(location: &u64) -> [CompressedRistretto;2] {
        let mut byte = [0u8;64];
        let mut r = BufReader::new(File::open(FILE_NAME).unwrap());
        r.seek(SeekFrom::Start(location*64)).expect("Seek failed");
        r.read(&mut byte).unwrap();
        [CompressedRistretto::from_slice(&byte[..32]),CompressedRistretto::from_slice(&byte[32..])] // OTAccount::summon_ota() from there
    }

    /// Get the information on the OTAccount at height location as raw bytes
    pub fn get_raw(location: &u64) -> [u8; 64] {
        let mut bytes = [0u8;64];
        let mut r = BufReader::new(File::open(FILE_NAME).unwrap());
        r.seek(SeekFrom::Start(location*64)).expect("Seek failed");
        r.read(&mut bytes).unwrap();
        bytes
    }

    /// Generate a OTAccount from the raw bytes
    pub fn read_raw(bytes: &Vec<u8>) -> OTAccount { // assumes the bytes start at the beginning
        OTAccount::summon_ota(&[CompressedRistretto::from_slice(&bytes[..32]),CompressedRistretto::from_slice(&bytes[32..64])]) // OTAccount::summon_ota() from there
    }

    /// Appends new OTAccounts to the file
    pub fn append(accs: &Vec<OTAccount>) {
        let buf = accs.into_iter().map(|x| [x.pk.compress().as_bytes().to_owned(),x.com.com.compress().as_bytes().to_owned()].to_owned()).flatten().flatten().collect::<Vec<u8>>();
        let mut f = OpenOptions::new().append(true).open(FILE_NAME).unwrap();
        f.write_all(&buf.iter().map(|x|*x).collect::<Vec<u8>>()).unwrap();

    }
}







#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::constants::PEDERSEN_H;


    #[test]
    fn multisignature_test() {
        use curve25519_dalek::scalar::Scalar;
        use crate::validation::MultiSignature;

        let message = "hi!!!".as_bytes().to_vec();
        let sk = (0..100).into_iter().map(|_| Scalar::from(rand::random::<u64>())).collect::<Vec<_>>();
        let pk = sk.iter().map(|x| x*PEDERSEN_H()).collect::<Vec<_>>();

        let x = sk.iter().map(|k| MultiSignature::gen_group_x(&k, &0).decompress().unwrap()).collect::<Vec<_>>();
        let xt = MultiSignature::sum_group_x(&x);
        let y = sk.iter().map(|k| MultiSignature::try_get_y(&k, &message, &xt, &0)).collect::<Vec<_>>();
        let yt = MultiSignature::sum_group_y(&y);



        assert!(MultiSignature::verify_group(&yt, &xt, &message, &pk.iter().map(|x| x.compress()).collect::<Vec<_>>()));
        assert!(MultiSignature::verify_group(&yt, &xt, &message, &pk.iter().rev().map(|x| x.compress()).collect::<Vec<_>>()));
        assert!(!MultiSignature::verify_group(&yt, &xt, &message, &pk[..99].iter().map(|x| x.compress()).collect::<Vec<_>>()));
        assert!(!MultiSignature::verify_group(&yt, &xt, &"bye!!!".as_bytes().to_vec(), &pk.iter().map(|x| x.compress()).collect::<Vec<_>>()));
    }

    #[test]
    fn message_signing_test() {
        use curve25519_dalek::scalar::Scalar;
        use crate::validation::Signature;

        let message = "hi!!!".as_bytes().to_vec();
        let sk = Scalar::from(rand::random::<u64>());
        let pk = (sk*PEDERSEN_H()).compress();
        let stkstate = vec![(pk,9012309183u64)];
        let mut m = Signature::sign_message(&sk,&message,&0u64);
        assert!(0 == Signature::recieve_signed_message(&mut m,&stkstate).unwrap());
        assert!(m == message);
    }

    #[test]
    fn message_signing_nonced_test() {
        use curve25519_dalek::scalar::Scalar;
        use crate::validation::Signature;

        let message = "hi!!!".as_bytes().to_vec();
        let sk = Scalar::from(rand::random::<u64>());
        let pk = (sk*PEDERSEN_H()).compress();
        let stkstate = vec![(pk,9012309183u64)];
        let mut m = Signature::sign_message_nonced(&sk,&message,&0u64,&80u64);
        assert!(0 == Signature::recieve_signed_message_nonced(&mut m,&stkstate,&80u64).unwrap());
        assert!(m == message);
    }

    #[test]
    fn many_block_time_calculations() {
        
        let seconds_per_year = 31_536_000f64;

        let runtime = Instant::now();
        let mut t0 = 0f64;
        for _ in 0..1_000_000 {
            let t = 1f64/(2f64*t0+2f64).ln();
            t0 += t/seconds_per_year;
        }
        println!("runtime: {}ms",runtime.elapsed().as_millis());
    }
}