// check-pass
#![feature(type_alias_impl_trait)]

fn opaque<'a: 'a>() -> impl Sized {}
fn assert_static<T: 'static>(_: T) {}

fn test_closure() {
    let closure = |_| {
        assert_static(opaque());
    };
    closure(&opaque());
}

type Opaque2 = impl Sized;
type Opaque<'a> = Opaque2;
fn define<'a>() -> Opaque<'a> {}

fn test_tait(_: &Opaque<'_>) {
    None::<&'static Opaque<'_>>;
}

fn main() {}
