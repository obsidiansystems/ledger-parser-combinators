//! The parsers in interp_parser divide "Schema" from "Interpretation".
//!
//! The core trait is [InterpParser], and that together with [DynInterpParser] form the public
//! interface of this module.
//!
//! Schema in this context describes the structure of the input; the "one byte of length and then
//! up to ten 32-bit numbers" part.
//!
//! Interpretation is then what we want to _do_ with the input; for ledger apps, this is usually
//! "cache it for a moment to prompt", "prompt for the return values of these sub-parsers" or
//! "forget it", corresponding to DefaultInterp, Action, and DropInterp.
//!
//! Arrays of values in the input we usually don't want to cache, as that would require either a
//! limit or non-constant memory; for those we handle them piece by piece with SubInterp.
use crate::core_parsers::*;
use crate::endianness::{Endianness, Convert};
use arrayvec::ArrayVec;

pub use crate::interp::*;

#[cfg(feature = "logging")]
use ledger_log::{trace,error};


/// Out-Of-Band messages; currently only Reject, but could plausibly be used to do prompts in a
/// co-routine way.
#[derive(PartialEq, Debug)]
pub enum OOB {
    // Prompt removed due to excessive memory use; we gain testability improvements if we can
    // reinstate an OOB for prompts and do the co-routine again, but we can't do that at this
    // memory use.
    //
    // Prompt([ArrayString<128>;2]),
    Reject
}

/// PResult stands for Partial Result
/// None = Incomplete
pub type PResult<T> = Option<T>;

/// This represents the part of the input that hasn't been yet consumed by the
/// parser, represented as a slice of the input.
pub type RemainingSlice<'a> = &'a [u8];


/// Return type for a parser.
///
/// If the parser completes its job correctly, we just need to return the remaining
/// slice. If the parser still needs data, it will return a None (and in that
/// case the remaining slice is empty because we consumed it all). If the parser
/// encounters an error condition, it will signal it in the OOB type, and we'll
/// return the remaining slice for further elaboration or resuming.
pub type ParseResult<'a> = Result<RemainingSlice<'a>, (PResult<OOB>, RemainingSlice<'a>)>;


pub fn reject<'a, R>(chunk: &'a [u8]) -> Result<R, (PResult<OOB>, &'a [u8])> {
    Err((Some(OOB::Reject), chunk))
}

pub fn need_more<'a, R>(chunk: &'a [u8]) -> Result<R, (PResult<OOB>, &'a [u8])> {
    Err((None, chunk))
}

// Core trait; describes an "interpretation" of a given datatype (specified with the types from
// core_parsers), which can have a variable return type and do stateful actions.

/// An InterpParser provides an actual parser between a schema `P` by interpretation mechanism
/// `Self`.
///
/// To use, first create a variable of type `State` by `init`, and initialize a destination to
/// None; and then repeatedly call `parse` with chunks of input until the result is `Ok(())`; at
/// that point destination will be filled. destination may become non-None at any point during the
/// parse, and should be a stable location.
///
/// For better memory usage during initialization in most cases, instead of using `init` declare a
/// `MaybeUninit` variable and use `init_in_place` to construct a state in it, then convert it to
/// `State`.

pub trait InterpParser<P> {
    type State;
    type Returning;
    fn init(&self) -> Self::State;
    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
        unsafe { (*state).as_mut_ptr().write(self.init()); }
    }
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a>;
}

/// DynInterpParser extends InterpParser by adding a method to set a parameter into the parser
/// as part of initialization.
///
/// This is mostly useful to avoid duplication of memory when implementing DynBind.

pub trait DynInterpParser<P>: InterpParser<P> {
    type Parameter;
    fn init_param(&self, params: Self::Parameter, state: &mut Self::State, destination: &mut Option<Self::Returning>);
}


pub struct ByteState;

#[inline(never)]
pub fn init_with_default<X: Default>(x: &mut Option<X>) {
    *x = Some(X::default());
}

#[inline(never)]
pub fn set_from_thunk<X, F: FnOnce() -> X>(x: &mut X, f: F) {
    *x = f();
}

#[inline(never)]
pub fn call_me_maybe<F: FnOnce() -> Option<()>>(f: F) -> Option<()> {
    f()
}
#[inline(never)]
pub fn call_me_maybe2<F: FnOnce() -> Option<()>>(f: F) -> Option<()> {
    f()
}

impl InterpParser<Byte> for DefaultInterp {
    type State = ByteState;
    type Returning = u8;
    fn init(&self) -> Self::State { Self::State {} }
    #[inline(never)]
    fn parse<'a, 'b>(&self, _state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        match chunk.split_first() {
            None => Err((None, chunk)),
            Some((first, rest)) => {
                *destination = Some(*first);
                Ok(rest)
            }
        }
    }
}

impl InterpParser<Byte> for DropInterp {
    type State = ();
    type Returning = ();
    fn init(&self) -> Self::State { () }
    #[inline(never)]
    fn parse<'a, 'b>(&self, _state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        match chunk.split_first() {
            None => Err((None, chunk)),
            Some((_, rest)) => {
                *destination = Some(());
                Ok(rest)
            }
        }
    }
}

pub struct ForwardArrayParserState<Item, SubparserState, const N : usize > {
    buffer: ArrayVec<Item, N>,
    // We want to let our subparser stream into it
    subparser_destination: Option<Item>,
    subparser_state: SubparserState
}


/* Note: we use this for parsing numbers. Additional requirement: don't stream into destination,
 * because number parser will be recreating destination each time. */
impl<I, S : InterpParser<I>, const N : usize> InterpParser<Array< I, N>> for SubInterp<S> {
    type State = ForwardArrayParserState<<S as InterpParser<I>>::Returning, <S as InterpParser<I>>::State, N>;
    type Returning = [<S as InterpParser<I>>::Returning; N];
    fn init(&self) -> Self::State {
        Self::State { buffer: ArrayVec::<<S as InterpParser<I>>::Returning,N>::new(),
                      subparser_destination: None,
                      subparser_state: <S as InterpParser<I>>::init(&self.0) }
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        let mut remaining : &'a [u8] = chunk;
        while !state.buffer.is_full() {
            match self.0.parse(&mut state.subparser_state, remaining, &mut state.subparser_destination)? {
                new_chunk => {
                    remaining = new_chunk;
                    state.buffer.push(core::mem::take(&mut state.subparser_destination).ok_or((Some(OOB::Reject), remaining))?);
                    state.subparser_state = <S as InterpParser<I>>::init(&self.0);
                }
            }
        }
        match state.buffer.take().into_inner() {
            Ok(rv) => {
                *destination = Some(rv);
                Ok(remaining)
            }
            Err(_) => Err((Some(OOB::Reject), remaining)) // Should be impossible, could just panic.
        }
    }
}

macro_rules! number_parser {
    ($p:ident, $size:expr, $t:ty) => {
        impl<const E: Endianness> InterpParser<$p<E>> for DefaultInterp where $t : Convert<E> {
            type State = <DefaultInterp as InterpParser<Array<Byte, $size>>>::State;
            type Returning = $t;
            fn init(&self) -> Self::State {
                <DefaultInterp as InterpParser<Array<Byte, $size>>>::init(&DefaultInterp)
            }
            #[inline(never)]
            fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
                let mut sub_destination : Option<[u8; $size]> = None;
                let remainder = <DefaultInterp as InterpParser<Array<Byte, $size>>>::parse(&DefaultInterp, state, chunk, &mut sub_destination)?;
                *destination = Some(Convert::<E>::deserialize((sub_destination.ok_or((Some(OOB::Reject), remainder))?)));
                Ok(remainder)
            }
        }
        impl<const E: Endianness> InterpParser<$p<E>> for DropInterp {
            type State = <SubInterp<DropInterp> as InterpParser<Array<Byte, $size>>>::State;
            type Returning = ();
            fn init(&self) -> Self::State {
                <SubInterp<DropInterp> as InterpParser<Array<Byte, $size>>>::init(&SubInterp(DropInterp))
            }
            #[inline(never)]
            fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
                let mut sub_destination : Option<[(); $size]> = None;
                let remainder = <SubInterp<DropInterp> as InterpParser<Array<Byte, $size>>>::parse(&SubInterp(DropInterp), state, chunk, &mut sub_destination)?;
                *destination = Some(());
                return Ok(remainder);
            }
        }
    }
}
number_parser! { U16, 2, u16 }
number_parser! { U32, 4, u32 }
number_parser! { U64, 8, u64 }

pub enum ForwardDArrayParserState<N, IS, I, const M : usize > {
    Length(N),
    Elements(ArrayVec<I, M>, usize, IS, Option<I>),
    Done
}

use core::convert::TryFrom;
impl<N, I, S : InterpParser<I>, const M : usize> InterpParser<DArray<N, I, M> > for SubInterp<S> where
    DefaultInterp : InterpParser<N>,
    usize: TryFrom<<DefaultInterp as InterpParser<N>>::Returning>,
    <S as InterpParser<I>>::Returning: Clone{
    type State=ForwardDArrayParserState<<DefaultInterp as InterpParser<N>>::State, <S as InterpParser<I>>::State, <S as InterpParser<I>>::Returning, M>;
    type Returning = ArrayVec<<S as InterpParser<I>>::Returning, M>;
    fn init(&self) -> Self::State {
        Self::State::Length(<DefaultInterp as InterpParser<N>>::init(&DefaultInterp))
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        use ForwardDArrayParserState::*;
        let mut cursor : &'a [u8] = chunk;
        loop {
            match state {
                Length(ref mut nstate) => {
                    let mut sub_destination : Option<<DefaultInterp as InterpParser<N>>::Returning> = None;
                    let newcur : &'a [u8] = <DefaultInterp as InterpParser<N>>::parse(&DefaultInterp, nstate, chunk, &mut sub_destination)?;
                    let len_temp = sub_destination.ok_or((Some(OOB::Reject), newcur))?;
                    cursor = newcur;
                    let len = <usize as TryFrom<<DefaultInterp as InterpParser<N>>::Returning>>::try_from(len_temp).or(Err((Some(OOB::Reject), newcur)))?;
                    set_from_thunk(state, || Elements(ArrayVec::new(), len, <S as InterpParser<I>>::init(&self.0), None));
                }
                Elements(ref mut vec, len, ref mut istate, ref mut sub_destination) => {
                    while vec.len() < *len {
                        cursor = self.0.parse(istate, cursor, sub_destination)?;
                        vec.try_push(core::mem::take(sub_destination).ok_or((Some(OOB::Reject), cursor))?).or(Err((Some(OOB::Reject), cursor)))?;
                        *istate = <S as InterpParser<I>>::init(&self.0);
                    }
                    *destination = match core::mem::replace(state, Done) { Elements(vec, _, _, _) => Some(vec), _ => break Err((Some(OOB::Reject), cursor)), };
                    break Ok(cursor);
                }
                Done => { break Err((Some(OOB::Reject), cursor)); }
            }
        }
    }
}


impl< I, const N : usize >  InterpParser<Array<I, N>> for DefaultInterp where
    DefaultInterp : InterpParser<I> {
    type State = <SubInterp<DefaultInterp> as InterpParser<Array< I, N> >>::State;
    type Returning = <SubInterp<DefaultInterp> as InterpParser<Array< I, N> >>::Returning;
    fn init(&self) -> Self::State {
        <SubInterp<DefaultInterp> as InterpParser<Array<I, N>>>::init(&SubInterp(DefaultInterp))
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        <SubInterp<DefaultInterp> as InterpParser<Array<I, N>>>::parse(&SubInterp(DefaultInterp), state, chunk, destination)
    }
}


/* // TODO: determine why this doesn't work.
impl< N, I, const M : usize> InterpParser<DArray<N, I, M>> for DefaultInterp where
    DefaultInterp : InterpParser<I> + InterpParser<N>, 
    usize: From<<DefaultInterp as InterpParser<N>>::Returning> {
    type State = <SubInterp<DefaultInterp> as InterpParser<DArray< N, I, M> > >::State;
    type Returning = <SubInterp<DefaultInterp> as InterpParser<DArray< N, I, M> > >::Returning;
    fn init(&self) -> Self::State {
        <SubInterp<DefaultInterp> as InterpParser<DArray<N, I, M>>>::init(&SubInterp(DefaultInterp))
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8]) -> ParseResult<'a> {
        <SubInterp<DefaultInterp> as InterpParser<DArray<N, I, M>>>::parse(&SubInterp(DefaultInterp), state, chunk)
    }
}
*/

impl<A, R, S : InterpParser<A>> InterpParser<A> for Action<S, fn(&<S as InterpParser<A>>::Returning, &mut Option<R>) -> Option<()>>
{
    type State = (<S as InterpParser<A> >::State, Option<<S as InterpParser<A> >::Returning>);
    type Returning = R;
    
    #[inline(never)]
    fn init(&self) -> Self::State {
        (<S as InterpParser<A>>::init(&self.0), None)
    }

    #[inline(never)]
    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
       self.0.init_in_place(unsafe { core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).0) as *mut core::mem::MaybeUninit<<S as InterpParser<A> >::State> });
       call_fn( || unsafe { (core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).1) as *mut Option<<S as InterpParser<A> >::Returning> ).write(None)} );
    }

    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        let new_chunk = self.0.parse(&mut state.0, chunk, &mut state.1)?;
        match (self.1)(state.1.as_ref().ok_or((Some(OOB::Reject),new_chunk))?, destination) {
            None => { Err((Some(OOB::Reject),new_chunk)) }
            Some(()) => { Ok(new_chunk) }
        }
    }
}

impl<A, R, S : DynInterpParser<A>> DynInterpParser<A> for Action<S, fn(&<S as InterpParser<A>>::Returning, &mut Option<R>) -> Option<()>>
    {
        type Parameter = S::Parameter;
        #[inline(never)]
        fn init_param(&self, param: Self::Parameter, state: &mut Self::State, _destination: &mut Option<Self::Returning>) {
            set_from_thunk(&mut state.0, || <S as InterpParser<A>>::init(&self.0));
            set_from_thunk(&mut state.1, || None);
            self.0.init_param(param, &mut state.0, &mut state.1);
        }
    }

/// This impl exists to allow the _function_ of an Action to be the target of the parameter for
/// DynInterpParser, thus giving an escape hatch to thread a parameter past a non-parameterized
/// parser. Whether this should still be an Action as opposed to some other name is not immediately
/// clear.
impl<A, R, S : InterpParser<A>, C> InterpParser<A> for Action<S, fn(&<S as
    InterpParser<A>>::Returning, &mut Option<R>, C) -> Option<()>>
{
    type State = (<S as InterpParser<A> >::State, Option<<S as InterpParser<A> >::Returning>, Option<C>);
    type Returning = R;
    
    #[inline(never)]
    fn init(&self) -> Self::State {
        (<S as InterpParser<A>>::init(&self.0), None, None)
    }

    #[inline(never)]
    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
       self.0.init_in_place(unsafe { core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).0) as *mut core::mem::MaybeUninit<<S as InterpParser<A> >::State> });
       call_fn( || unsafe { (core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).1) as *mut Option<<S as InterpParser<A> >::Returning> ).write(None)} );
       call_fn( || unsafe { (core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).2) as *mut Option<C> ).write(None)} );
    }

    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        let new_chunk = self.0.parse(&mut state.0, chunk, &mut state.1)?;
        match (self.1)(state.1.as_ref().ok_or((Some(OOB::Reject),new_chunk))?, destination, core::mem::take(&mut state.2).ok_or((Some(OOB::Reject),new_chunk))?) {
            None => { Err((Some(OOB::Reject),new_chunk)) }
            Some(()) => { Ok(new_chunk) }
        }
    }
}

impl<A, R, S : InterpParser<A>, C> DynInterpParser<A> for Action<S, fn(&<S as InterpParser<A>>::Returning, &mut Option<R>, C) -> Option<()>>
    {
        type Parameter = C;
        #[inline(never)]
        fn init_param(&self, param: Self::Parameter, state: &mut Self::State, _destination: &mut Option<Self::Returning>) {
            set_from_thunk(&mut state.0, || <S as InterpParser<A>>::init(&self.0));
            set_from_thunk(&mut state.1, || None);
            set_from_thunk(&mut state.2, || Some(param));
        }
    }

impl<A, R, S : InterpParser<A>> InterpParser<A> for MoveAction<S, fn(<S as InterpParser<A>>::Returning, &mut Option<R>) -> Option<()>>
{
    type State = (<S as InterpParser<A> >::State, Option<<S as InterpParser<A> >::Returning>);
    type Returning = R;
    
    #[inline(never)]
    fn init(&self) -> Self::State {
        (<S as InterpParser<A>>::init(&self.0), None)
    }

    #[inline(never)]
    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
       self.0.init_in_place(unsafe { core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).0) as *mut core::mem::MaybeUninit<<S as InterpParser<A> >::State> });
       call_fn( || unsafe { (core::ptr::addr_of_mut!((*(*state).as_mut_ptr()).1) as *mut Option<<S as InterpParser<A> >::Returning> ).write(None)} );
    }

    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        let new_chunk = self.0.parse(&mut state.0, chunk, &mut state.1)?;
        match (self.1)(core::mem::take(&mut state.1).ok_or((Some(OOB::Reject),new_chunk))?, destination) {
            None => { Err((Some(OOB::Reject),new_chunk)) }
            Some(()) => { Ok(new_chunk) }
        }
    }
}

impl<A, R, S : DynInterpParser<A>> DynInterpParser<A> for MoveAction<S, fn(<S as InterpParser<A>>::Returning, &mut Option<R>) -> Option<()>>
    {
        type Parameter = S::Parameter;
        #[inline(never)]
        fn init_param(&self, param: Self::Parameter, state: &mut Self::State, _destination: &mut Option<Self::Returning>) {
            set_from_thunk(&mut state.0, || <S as InterpParser<A>>::init(&self.0));
            set_from_thunk(&mut state.1, || None);
            self.0.init_param(param, &mut state.0, &mut state.1);
        }
    }

fn rej<'a>(cnk: &'a [u8]) -> (PResult<OOB>, RemainingSlice<'a>) {
    (Some(OOB::Reject), cnk)
}


/// State for Bind
///
/// Initially the state is the state of the first subparser, and its result location
/// After the first subparser runs, if it failed, then the whole bind parser will fail
/// but if it succeeds, then the parser state transitions to BindSecond.
#[derive(InPlaceInit)]
pub enum BindState<A,B,S:InterpParser<A>,T:InterpParser<B>> {
    BindFirst(S::State, Option<<S as InterpParser<A> >::Returning>),
    BindSecond(T, <T as InterpParser<B>>::State)
}

impl<A, B, S : InterpParser<A>, T : InterpParser<B>> InterpParser<(A,B)> for Bind<S, fn(&<S as InterpParser<A>>::Returning) -> Option<T>>
{
    type State = BindState<A,B,S,T>;
    type Returning = <T as InterpParser<B>>::Returning;
    #[inline(never)]
    fn init(&self) -> Self::State {
        use BindState::*;
        // #[cfg(feature = "logging")]
        // error!("Bind T size: {} {}", core::mem::size_of::<T>(), core::mem::size_of::<T::State>());
        BindFirst (<S as InterpParser<A>>::init(&self.0), None)
    }

    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
        Self::State::init_bind_first(state, |a| <S as InterpParser<A>>::init_in_place(&self.0, a), |b| call_fn2(|| unsafe { (*b).as_mut_ptr().write(None); }));
    }

    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        use BindState::*;
        let mut cursor = chunk;
        loop {
            match state {
                BindFirst(ref mut s, ref mut r) => {
                    cursor = self.0.parse(s, cursor, r)?;
                    let r_temp = core::mem::take(r);
                    call_me_maybe(|| {
                        let next = self.1(r_temp.as_ref()?)?;
                        let next_state = next.init();
                        *state = BindSecond(next, next_state);
                        Some(())
                    }).ok_or((Some(OOB::Reject), cursor))?;
                }
                BindSecond(t, ref mut s) => {
                    cursor = t.parse(s, cursor, destination)?;
                    return Ok(cursor);
                }
            }
        }
    }
}

#[derive(InPlaceInit)]
#[repr(u8)]
pub enum DynBindState<A,B,S:InterpParser<A>,T:InterpParser<B>> {
    BindFirst(S::State, Option<<S as InterpParser<A> >::Returning>),
    BindSecond(<T as InterpParser<B>>::State)
}

#[inline(never)]
fn call_fn(f: impl FnOnce()) {
    f()
}
#[inline(never)]
fn call_fn2(f: impl FnOnce()) {
    f()
}

impl<A, B, S : InterpParser<A>, T : DynInterpParser<B, Parameter = S::Returning>> InterpParser<(A,B)> for DynBind<S, T>
// fn(&<S as InterpParser<A>>::Returning) -> Option<T>>
{
    type State = DynBindState<A,B,S,T>;
    type Returning = <T as InterpParser<B>>::Returning;
    #[inline(never)]
    fn init(&self) -> Self::State {
        use DynBindState::*;
        // #[cfg(feature = "logging")]
        // error!("Bind T size: {} {}", core::mem::size_of::<T>(), core::mem::size_of::<T::State>());
        BindFirst (<S as InterpParser<A>>::init(&self.0), None)
    }
    
    #[inline(never)]
    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
        Self::State::init_bind_first(state, |a| call_fn2(|| <S as InterpParser<A>>::init_in_place(&self.0, a)), |b| call_fn2(|| unsafe { (*b).as_mut_ptr().write(None); }));
    }

    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        use DynBindState::*;
        let mut cursor = chunk;
        loop {
            match state {
                BindFirst(ref mut s, ref mut r) => {
                    cursor = self.0.parse(s, cursor, r)?;
                    call_me_maybe(|| {
                        let r_temp;
                        if let BindFirst(_, ref mut r) = state {
                            r_temp = core::mem::take(r)?;
                        } else {
                            unreachable!();
                        }
                        Self::State::init_bind_second(unsafe { core::mem::transmute(state as *mut Self::State) }, |a| call_fn(|| self.1.init_in_place(a)));
                        if let BindSecond(ref mut s) = state {
                            self.1.init_param(r_temp, s, destination);
                        } else {
                            unreachable!();
                        }
                        Some(())
                        /* call_me_maybe2(|| {
                            // *state = BindSecond(self.1.init());
                            match state {
                                BindSecond(ref mut s) => call_fn(|| self.1.init_param(r_temp, s, destination)),
                                _ => return None,
                            };
                            Some(())
                        })*/
                    }).ok_or((Some(OOB::Reject), cursor))?;
                }
                BindSecond(ref mut s) => {
                    cursor = self.1.parse(s, cursor, destination)?;
                    return Ok(cursor);
                }
            }
        }
    }
}

impl<A, B, S: DynInterpParser<A>, T: DynInterpParser<B, Parameter = S::Returning>> DynInterpParser<(A,B)> for DynBind<S, T>
    {
        type Parameter = S::Parameter;
        #[inline(never)]
        fn init_param(&self, param: Self::Parameter, state: &mut Self::State, _destination: &mut Option<Self::Returning>) {
            self.init_in_place(unsafe { core::mem::transmute(state as *mut Self::State) });
            // *state = DynBindState::BindFirst(<S as InterpParser<A>>::init(&self.0), None);
            match state {
                DynBindState::BindFirst(ref mut s, ref mut sub_destination) => self.0.init_param(param, s, sub_destination),
                _ => unreachable!(),
            }
        }
    }

impl<A, X : Clone, F : Fn(&mut X, &[u8])->(), S : InterpParser<A>> InterpParser<A> for ObserveBytes<X, F, S>
    {
    type State = Option<<S as InterpParser<A>>::State>;
    // Making a compromise here; if we return our sub-parser's result still wrapped in Option, we
    // can avoid storing it in our own state and then copying.
    type Returning = (X, Option<<S as InterpParser<A>>::Returning>);
    #[inline(never)]
    fn init(&self) -> Self::State {
        None
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        loop {
            break match state {
                None => {
                    *destination = Some(((self.0)(), None));
                    set_from_thunk(state, || Some(<S as InterpParser<A>>::init(&self.2)));
                    continue;
                }
                Some(ref mut subparser_state) => {
                    let new_chunk = <S as InterpParser<A>>::parse(&self.2, subparser_state, chunk, &mut destination.as_mut().ok_or(rej(chunk))?.1)?;
                    self.1(&mut destination.as_mut().ok_or(rej(new_chunk))?.0, &chunk[0..chunk.len()-new_chunk.len()]);
                    Ok(new_chunk)
                }
            }
        }
    }
}

impl<A, X:Clone, F: Fn(&mut X, &[u8])->(), S: InterpParser<A>> DynInterpParser<A> for ObserveBytes<X, F, S>
    {
        type Parameter = X;
        #[inline(never)]
        fn init_param(&self, param: Self::Parameter, state: &mut Self::State, destination: &mut Option<Self::Returning>) {
            *destination = Some((param.clone(), None));
            *state = Some(<S as InterpParser<A>>::init(&self.2));
        }
    }

pub enum PairState<A, B> {
    Init,
    First(A),
    Second(B),
}

/// Pairs of parsers parse the sequence of their schema types, and return a pair of their results.
impl<A : InterpParser<C>, B : InterpParser<D>, C, D> InterpParser<(C, D)> for (A, B) {
    type State = PairState<<A as InterpParser<C>>::State, <B as InterpParser<D>>::State>;
    type Returning = (Option<A::Returning>, Option<B::Returning>);
    #[inline(never)]
    fn init(&self) -> Self::State {
        PairState::Init
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        let mut cursor = chunk;
        loop {
            match state {
                PairState::Init => {
                    init_with_default(destination);
                    set_from_thunk(state, || PairState::First(<A as InterpParser<C>>::init(&self.0)));
                }
                PairState::First(ref mut sub) => {
                    cursor = <A as InterpParser<C> >::parse(&self.0, sub, cursor, &mut destination.as_mut().ok_or(rej(cursor))?.0)?;
                    set_from_thunk(state, || PairState::Second(<B as InterpParser<D>>::init(&self.1)));
                }
                PairState::Second(ref mut sub) => {
                    cursor = <B as InterpParser<D> >::parse(&self.1, sub, cursor, &mut destination.as_mut().ok_or(rej(cursor))?.1)?;
                    break Ok(cursor);
                }
            }
        }
    }
}

/*
 // TODO: handle struct-like data structures without using the pair parser above and with named
 // fields.
 //
#[macro_export]
macro_rules! def_table {
    {struct $name:ident { $($fieldName:ident : $type:ty),+ } } => 
    {
        struct $name<$($fieldName),+> {
            $($fieldName: $fieldName),+
        }
    }

    enum 
    impl<$($fieldName : InterpParser<$type>),+> InterpParser<$name<$($fieldName),+>> for $name<$($fieldName),+> {

    }
}
*/

#[derive(InPlaceInit)]
pub enum LengthFallbackParserState<N, NO, IS> {
    Length(N, NO),
    Element(usize, usize, IS),
    Failed(usize, usize),
    Done
}

// First step, sketch out the states of your parser, with your transitions in mind
pub struct LengthLimitedState<State> {
    bytes_seen : usize,
    child_state : State,
}

// Now define the parser type, which will resemble the mirror image of the state
// Defined in interp.rs:
//
// #[derive(Clone)]
// pub struct LengthLimited<S> {
//    bytes_limit : usize,
//    subparser : S
//}

// Implement InterpParser for the parser
impl<I, S : InterpParser<I>> InterpParser<I> for LengthLimited<S> {
    type State=LengthLimitedState<<S as InterpParser<I>>::State>;
    type Returning = <S as InterpParser<I>>::Returning;
    // Init is usually fairly straightforward
    fn init(&self) -> Self::State {
        LengthLimitedState {
            bytes_seen: 0,
            child_state: self.subparser.init()
        }
    }
    // Start by typing out the type signature, copying the input slice into a mutable reference
    // and successfully return the cursor. Elaborate on the parser from there.
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        let feed_amount = core::cmp::min(chunk.len(), self.bytes_limit - state.bytes_seen);
        // If you're calling a subparser, you will probably want to match on its status
        // Note that we are trying to keep _our_ state in lockstep with the state of our child.
        // If the child consumes, we account for it, even if we end up in a bad state.
        match self.subparser.parse(&mut state.child_state, &chunk[0..feed_amount], destination) {
            Ok(new_cursor) => {
                let consumed = feed_amount - new_cursor.len();
                state.bytes_seen += consumed;
                // If our child has accepted, they better have eaten all their vegetables.
                if consumed < feed_amount || state.bytes_seen < self.bytes_limit {
                    return Err((Some (OOB::Reject), new_cursor));
                }
                return Ok(&chunk[feed_amount..chunk.len()]);
            }
            Err((None, new_cursor)) => {
                let consumed = feed_amount - new_cursor.len();
                state.bytes_seen += consumed;
                // How can you have any pudding if you don't eat your meat?
                if consumed < feed_amount || state.bytes_seen >= self.bytes_limit {
                    return Err((Some (OOB::Reject), new_cursor));
                }
                Err((None, new_cursor))
            }
            Err((w, new_cursor)) => {
                let consumed = feed_amount - new_cursor.len();
                state.bytes_seen += consumed;
                Err((w, new_cursor))
            }
        }
    }
}


impl<IFun : Fn () -> X, N, I, S : InterpParser<I>, X, F: Fn(&mut X, &[u8])->()> InterpParser<LengthFallback<N, I>> for ObserveLengthedBytes<IFun, X, F, S> where
    DefaultInterp : InterpParser<N>,
    usize: TryFrom<<DefaultInterp as InterpParser<N>>::Returning>,
    <DefaultInterp as InterpParser<N>>::Returning: Copy {
    type State=LengthFallbackParserState<<DefaultInterp as InterpParser<N>>::State, Option<<DefaultInterp as InterpParser<N>>::Returning>, <S as InterpParser<I>>::State>;
    type Returning = (Option<<S as InterpParser<I>>::Returning>, X);
    #[inline(never)]
    fn init(&self) -> Self::State {
        LengthFallbackParserState::Length(<DefaultInterp as InterpParser<N>>::init(&DefaultInterp), None)
    }
    #[inline(never)]
    fn init_in_place(&self, state: *mut core::mem::MaybeUninit<Self::State>) {
        Self::State::init_length(state, |a| <DefaultInterp as InterpParser<N>>::init_in_place(&DefaultInterp, a), |b| call_fn( || unsafe { (*b).as_mut_ptr().write(None); }));
    }
    #[inline(never)]
    fn parse<'a, 'b>(&self, state: &'b mut Self::State, chunk: &'a [u8], destination: &mut Option<Self::Returning>) -> ParseResult<'a> {
        use LengthFallbackParserState::*;
        let mut cursor : &'a [u8] = chunk;
        loop {
            break match state {
                Length(ref mut nstate, ref mut length_out) => {
                    cursor = <DefaultInterp as InterpParser<N>>::parse(&DefaultInterp, nstate, cursor, length_out)?;
                    let len = <usize as TryFrom<<DefaultInterp as InterpParser<N>>::Returning>>::try_from(length_out.ok_or(rej(cursor))?).or(Err(rej(cursor)))?;
                    match destination {
                      None => {
                          /*call_me_maybe(|| {
                          let result = self.0();
                          *destination = Some((None, result));
                          Some(())
                          }).ok_or(rej(cursor))?;*/
                      }
                      _ => { }
                    }
                    set_from_thunk(state, || Element(0, len, <S as InterpParser<I>>::init(&self.2)));
                    continue;
                }
                Element(ref mut consumed, len, ref mut istate) => {
                    let passed_cursor = &cursor[0..core::cmp::min(cursor.len(), (*len)-(*consumed))];
                    match self.2.parse(istate, passed_cursor, &mut destination.as_mut().ok_or(rej(cursor))?.0) {
                        Ok(new_cursor) => {
                            let consumed_from_chunk = passed_cursor.len() - new_cursor.len();
                            *consumed += consumed_from_chunk;
                            self.1(&mut destination.as_mut().ok_or(rej(cursor))?.1, &cursor[0..passed_cursor.len()-new_cursor.len()]);
                            if *consumed == *len {
                                Ok(&cursor[consumed_from_chunk..])
                            } else {
                                cursor = new_cursor;
                                destination.as_mut().ok_or(rej(cursor))?.0 = None;
                                let cv = *consumed;
                                let lv = *len;
                                set_from_thunk(state, || Failed(cv, lv));
                                continue;
                            }
                        }
                        Err((None, new_cursor)) => {
                            let consumed_from_chunk = passed_cursor.len() - new_cursor.len();
                            *consumed += consumed_from_chunk;
                            self.1(&mut destination.as_mut().ok_or(rej(cursor))?.1, &cursor[0..passed_cursor.len()-new_cursor.len()]);
                            if *consumed == *len {
                                Ok(&cursor[consumed_from_chunk..])
                            } else {
                                Err((None, &cursor[consumed_from_chunk..]))
                            }
                        }
                        Err((Some(OOB::Reject), _)) => {
                            let cv = *consumed;
                            let lv = *len;
                            set_from_thunk(state, || Failed(cv, lv));
                            continue;
                        }
                    }
                }
                Failed(ref mut consumed, len) => {
                    if self.3 {
                        write!(DBG, "We hit a failed state in the parser\n").or(Err(rej(cursor)))?;
                        return Err((Some(OOB::Reject), cursor));
                    } else {
                        use core::cmp::min;
                        let new_cursor = &cursor[min((*len) - (*consumed), cursor.len())..];
                        self.1(&mut destination.as_mut().ok_or(rej(cursor))?.1, &cursor[0..cursor.len()-new_cursor.len()]);
                        if cursor.len() >= ((*len) - (*consumed)) {
                            set_from_thunk(state, || Done);
                            set_from_thunk(&mut destination.as_mut().ok_or(rej(cursor))?.0, || None);
                            Ok(new_cursor)
                        } else {
                            let new_consumed = *consumed + cursor.len();
                            let new_len = *len;
                            set_from_thunk(state, || Failed(new_consumed, new_len));
                            Err((None, new_cursor))
                        }
                    }
                }
                Done => { Err((Some(OOB::Reject), cursor)) }
            }
        }
    }
}

impl<IFun : Fn () -> X, N, I, S : InterpParser<I>, X, F: Fn(&mut X, &[u8])->()> DynInterpParser<LengthFallback<N, I>> for ObserveLengthedBytes<IFun, X, F, S> where
    DefaultInterp : InterpParser<N>,
    usize: TryFrom<<DefaultInterp as InterpParser<N>>::Returning>,
    <DefaultInterp as InterpParser<N>>::Returning: Copy {
        type Parameter = X;
        #[inline(never)]
        fn init_param(&self, param: Self::Parameter, state: &mut Self::State, destination: &mut Option<Self::Returning>) {
            set_from_thunk(destination, || { Some((None, param)) });
            // *destination = Some((None, param.clone()));
            *state = LengthFallbackParserState::Length(<DefaultInterp as InterpParser<N>>::init(&DefaultInterp), None)
        }
    }

    pub struct DBG;
    use core;
    #[allow(unused_imports)]
    use core::fmt::Write;
    impl core::fmt::Write for DBG {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            use arrayvec::ArrayString;
            let mut qq = ArrayString::<128>::new();
            qq.push_str(s);
            #[cfg(target_os="nanos")]
            nanos_sdk::debug_print(qq.as_str());
            #[cfg(not(target_os="nanos"))]
            std::print!("{}", qq.as_str());
            Ok(())
        }
    }

/*
#[cfg(test)]
mod test {

#[cfg(all(target_os="nanos", test))]
    use testmacro::test_item as test;
#[cfg(all(target_os="nanos", test))]
#[allow(unused_imports)]
    use nanos_sdk::{TestType, debug_print}; // , Pic};
#[cfg(all(not(target_os="nanos"), test))]
    fn debug_print(_s: &str) {
    }

    struct DBG;
    use core;
    #[allow(unused_imports)]
    use core::fmt::Write;
    impl core::fmt::Write for DBG {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            use arrayvec::ArrayString;
            let mut qq = ArrayString::<128>::new();
            qq.push_str(s);
            debug_print(qq.as_str());
            Ok(())
        }
    }

    use core::fmt::Debug;
    use super::{InterpParser, DefaultInterp, SubInterp, Action, ObserveBytes, OOB};
    #[allow(unused_imports)]
    use crate::core_parsers::{Byte, Array, DArray, U16, U32 };
    #[allow(unused_imports)]
    use arrayvec::ArrayVec;

    fn parser_test_feed<P, T: InterpParser<P>, RT: Debug + ?Sized>(parser: T, chunks: &[&[u8]], result: &RT, oobs: &[OOB]) where T::Returning: PartialEq<RT> + Debug
    {
        let mut oob_iter = oobs.iter();
        let mut chunk_iter = chunks.iter();
        let mut cursor : &[u8] = chunk_iter.next().unwrap();
        let mut parser_state = T::init(&parser);
        let mut destination : Option<RT>;
        loop {
            match <T as InterpParser<P>>::parse(&parser, &mut parser_state, cursor, &mut destination) {
                Err((Some(o), _new_cursor)) => {
                    assert_eq!(Some(&o), oob_iter.next());
                    match o {
                        OOB::Reject => {
                            assert_eq!(oob_iter.next(), None);
                            assert_eq!(chunk_iter.next(), None);
                            break;
                        }
                        // If there are any non-Reject OOB options uncomment this.
                        // _ => {
                        //    cursor = new_cursor; // Not sure why rustc claims this is unused.
                        //}
                    }
                }
                Err((None, new_cursor)) => {
                    assert_eq!(new_cursor, &[][..]);
                    match chunk_iter.next() {
                        Some(new) => { 
                            cursor = new;
                        }
                        None => {
                            panic!("Ran out of input chunks before parser accepted");
                        } } }
                Ok(new_cursor) => {
                    assert_eq!(destination.as_ref().unwrap(), result);
                    assert_eq!(new_cursor, &[][..]);
                    assert_eq!(chunk_iter.next(), None);
                    assert_eq!(oob_iter.next(), None);
                    break;
                }
            }
        }
    }
/*
#[test]
fn byte_parser() {
    let mut state = <DefaultInterp as InterpParser<Byte>>::init(&DefaultInterp);
    assert_eq!(<DefaultInterp as InterpParser<Byte>>::parse(&DefaultInterp, &mut state, b"cheez"), Ok((b'c', &b"heez"[..])));
    assert_eq!(<DefaultInterp as InterpParser<Byte>>::parse(&DefaultInterp, &mut state, b""), Err((None, &b""[..])));

}
*/

    fn init_parser<A, P: InterpParser<A>>(p: &P) -> <P as InterpParser<A>>::State {
        <P as InterpParser<A>>::init(p)
    }
    fn run_parser<'a, 'b, A, P: InterpParser<A>>(p: &P, state: &'b mut <P as InterpParser<A>>::State, chunk: &'a [u8]) -> Result<(<P as InterpParser<A>>::Returning, super::RemainingSlice<'a>), (super::PResult<OOB>, super::RemainingSlice<'a>)> {
        let mut destination : Option<<P as InterpParser<A>>::Returning> = None;
        <P as InterpParser<A>>::parse(p, state, chunk).map(|_| destination.unwrap());
    }

/*
#[test]
fn interp_byte_parser() {
    let p = super::Action(DefaultInterp, |x: &u8, d: &mut Option<u8>| { *d = Some(*x); Some(()) });
    let mut state = init_parser::<Byte,_>(&p);
    assert_eq!(run_parser::<Byte,_>(&p, &mut state, b"cheez"), Ok((b'c', &b"heez"[..])));
}
*/

/*
#[test]
fn test_length_fallback() {
    type Format = super::LengthFallback<Byte, Array<Byte, 5>>;
    parser_test_feed::<Format, _, _>(super::ObserveLengthedBytes(|| { ArrayVec::<u8, 5>::new() }, |a: &mut ArrayVec<u8, 5> , b: &[u8]| { let _ = a.try_extend_from_slice(b); }, DefaultInterp), &[b"\x05fooba"], &(Ok(*(b"fooba")), (*(b"fooba")).into()), &[]);
    use crate::endianness::{Endianness};
    type Format2 = super::LengthFallback<U32< { Endianness::Little } >, Array<Byte, 5>>;
    parser_test_feed::<Format2, _, _>(super::ObserveLengthedBytes(|| { ArrayVec::<u8, 5>::new() }, |a: &mut ArrayVec<u8, 5> , b: &[u8]| { let _ = a.try_extend_from_slice(b); }, DefaultInterp), &[b"\x05\x00\x00\x00fooba"], &(Ok(*(b"fooba")), (*(b"fooba")).into()), &[]);
    parser_test_feed::<Format2, _, _>(super::ObserveLengthedBytes(|| { ArrayVec::<u8, 6>::new() }, |a: &mut ArrayVec<u8, 6> , b: &[u8]| { let _ = a.try_extend_from_slice(b); }, DefaultInterp), &[b"\x06\x00\x00\x00foobar"], &(Err(()), (*(b"foobar")).into()), &[]);
    parser_test_feed::<Format2, _, _>(super::ObserveLengthedBytes(|| { ArrayVec::<u8, 7>::new() }, |a: &mut ArrayVec<u8, 7> , b: &[u8]| { let _ = a.try_extend_from_slice(b); }, DefaultInterp), &[b"\x07\x00\x00\x00foobarb"], &(Err(()), (*(b"foobarb")).into()), &[]);
    parser_test_feed::<Format2, _, _>(super::ObserveLengthedBytes(|| { ArrayVec::<u8, 4>::new() }, |a: &mut ArrayVec<u8, 4> , b: &[u8]| { let _ = a.try_extend_from_slice(b); }, DefaultInterp), &[b"\x04\x00\x00\x00foob"], &(Err(()), (*(b"foob")).into()), &[]);
}

#[test]
fn test_array() {
    let p = super::DefaultInterp;
    parser_test_feed::<Array<Byte,5>, super::DefaultInterp, _>(p, &[b"foo",b"ba"], &b"fooba"[..], &[])
}

#[test]
    fn test_darray() {
        parser_test_feed::<DArray<Byte,Byte,5>, _, _>(SubInterp(DefaultInterp), &[b"\0"], &b""[..], &[]);
        parser_test_feed::<DArray<Byte,Byte,5>, _, _>(SubInterp(DefaultInterp), &[b"\x05abcde"], &b"abcde"[..], &[]);
        parser_test_feed::<DArray<Byte,Byte,5>, _, _>(SubInterp(Action(DefaultInterp, |_: &u8| Some(()))), &[b"\x05abcde"], &[(),(),(),(),()][..], &[]);
        let obs = ObserveBytes(
            || 0, |a : &mut usize,b : &[u8]| { *a += b.len(); },
            SubInterp(
                Action(DefaultInterp, |_: &u8| Some(()))));
        parser_test_feed::<DArray<Byte,Byte,5>, _, _>(obs, &[b"\x05abcde"], &(6, (ArrayVec::from([(),(),(),(),()]))), &[]);
    }
*/

}
*/