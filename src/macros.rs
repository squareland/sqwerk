pub use paste::paste;

#[macro_export]
macro_rules! packet {
    ($vis: vis enum $ty_name: ident { $($name:ident {$($field:ident : $ty: ty),*} ),* }) => {
        $(
            #[derive(serde::Serialize, serde::Deserialize, Debug)]
            $vis struct $name {
                $($vis $field : $ty),*
            }
        )*

        #[derive(serde::Serialize, serde::Deserialize, Debug)]
        $vis enum $ty_name {
            $(
                $name($name),
            )*
        }

        $crate::macros::paste! {
            impl $ty_name {
                $vis fn handle_by<H>(self, handler: &mut H) where H: [<$ty_name Handler>] {
                    match self {
                        $(
                            Self :: $name(v) => handler.[<handle_ $name:snake>] (v)
                        ),*
                    }
                }

                $(
                    $vis fn [<$name:snake>] ($($field : $ty),*) -> Self {
                        Self :: $name($name {
                            $($field),*
                        })
                    }
                )*
            }

            $vis trait [<$ty_name Handler>] {
                $(
                    fn [<handle_ $name:snake>] (&mut self, packet: $name) {
                        //NOOP
                    }
                )*
            }
        }
    };
}