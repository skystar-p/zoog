use derivative::Derivative;

use crate::comment_list::CommentList;
use crate::header_rewriter::{self, HeaderRewrite};
use crate::opus::{CommentHeader, DiscreteCommentList, OpusHeader};
use crate::Error;

/// Mode type for `CommentRewriter`
#[derive(Derivative)]
#[derivative(Debug)]
pub enum CommentRewriterAction {
    NoChange,
    Modify {
        #[derivative(Debug = "ignore")]
        retain: Box<dyn Fn(&str, &str) -> bool>,
        append: DiscreteCommentList,
    },
    Replace(DiscreteCommentList),
}

/// Configuration type for `CommentRewriter`
#[derive(Debug)]
pub struct CommentRewriterConfig {
    /// The action to be performed
    pub action: CommentRewriterAction,
}

/// Parameterization struct for `HeaderRewriter` to rewrite ouput gain and R128
/// tags.
#[derive(Debug)]
pub struct CommentHeaderRewrite {
    config: CommentRewriterConfig,
}

impl CommentHeaderRewrite {
    pub fn new(config: CommentRewriterConfig) -> CommentHeaderRewrite { CommentHeaderRewrite { config } }
}

impl HeaderRewrite for CommentHeaderRewrite {
    type Error = Error;
    type Summary = DiscreteCommentList;

    fn summarize(
        &self, _opus_header: &OpusHeader, comment_header: &CommentHeader,
    ) -> Result<DiscreteCommentList, Error> {
        Ok(comment_header.to_discrete_comment_list())
    }

    fn rewrite(&self, _opus_header: &mut OpusHeader, comment_header: &mut CommentHeader) -> Result<(), Error> {
        match &self.config.action {
            CommentRewriterAction::NoChange => {}
            CommentRewriterAction::Replace(tags) => {
                comment_header.clear();
                comment_header.extend(tags.iter())?;
            }
            CommentRewriterAction::Modify { retain, append } => {
                comment_header.retain(retain);
                comment_header.extend(append.iter())?;
            }
        }
        Ok(())
    }
}

/// The result type of submitting a packet
pub type SubmitResult = header_rewriter::SubmitResult<DiscreteCommentList>;
